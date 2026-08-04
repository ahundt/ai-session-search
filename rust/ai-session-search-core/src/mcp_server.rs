// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};

use crate::config::Config;
use crate::dates::{self, Bound};
use crate::db::{Db, QueryCancellation};
use crate::inspect::InspectionOptions;
use crate::message_search::{
    selected_field_view, ContextWindow, DetailLevel, FieldViewBudget, LineWindow, MatchViewBudget,
    MatchWindow, MessageContentExtent, MessageQuery, MessageSearchInclude, MessageSearchParameter,
    MessageSearchRequest, MessageSearchResponse, MessageTarget, PurposeSelection, ReceiptLevel,
    RequestedExtent, RequestedTimeRange, ResolvedRequestExtent, SearchSurface, SequenceRange,
    DEFAULT_MATCH_EVIDENCE_MAX_CHARS,
};
use crate::models::{MessageFilters, Provider, Role, SearchFilters, SessionRecord};
use crate::refs::{extract_refs_from_text, ref_summary};
use crate::runtime::ExecutionRuntime;
use crate::service::SessionSearch;
use crate::service::{CatalogService, MessageService};
use crate::sql_query::{self, DbSchemaArgs, ResolvedDbQueryArgs};
use crate::util::{
    current_repo, normalize_path_prefix, render_posix_shell_command, resume_plan,
    select_transcript_lines, truncate_for_display,
};

/// Context radius in the generated one-call `get_session` continuation for a message hit.
const GET_SESSION_FOLLOW_UP_CONTEXT: i64 = 5;

/// MCP's text channel is a navigation aid, not a second serialization of structured results.
///
/// Keeping both limits constant makes retained summary memory `O(1)` with respect to result count
/// and field size. Exact match text and every returned result remain available in
/// `structuredContent`.
const MESSAGE_SEARCH_TEXT_RESULT_LIMIT: usize = 10;
const MESSAGE_SEARCH_TEXT_FIELD_CHARS: usize = 160;

/// Serve MCP over standard input/output through the official `rmcp` transport until EOF.
pub fn serve() -> anyhow::Result<()> {
    serve_with_config(Config::load()?)
}

/// Serve with configuration already resolved by an embedding CLI or API.
pub fn serve_with_config(config: Config) -> anyhow::Result<()> {
    tokio::runtime::Runtime::new()?.block_on(async move {
        OfficialMcpServer::new(config)?
            .serve_transport(rmcp::transport::stdio())
            .await?
            .waiting()
            .await?;
        Ok(())
    })
}

/// Shared state behind the official MCP SDK adapter.
///
/// The SDK owns JSON-RPC framing and connection lifecycle. This state owns only lazily opened
/// application data, validated client roots, the generated tool catalogue, and refresh work.
struct McpState {
    config: Config,
    app: Option<SessionSearch>,
    harness_roots: Vec<std::path::PathBuf>,
    roots_error: Option<String>,
    advertised_tools: Option<Value>,
    refresh_worker: RefreshWorker,
}

/// Official MCP SDK adapter around AI Session Search's transport-neutral state and tool semantics.
///
/// `rmcp` owns JSON-RPC framing, protocol negotiation, request concurrency, and cancellation.
/// The mutex protects preparation, refresh scheduling, the tool catalogue, and root-authority
/// state. Each tool call releases that mutex before opening an independent prepared SQLite reader
/// and moving blocking database work off the async runtime.
pub struct OfficialMcpServer {
    inner: Arc<Mutex<McpState>>,
    reader_runtime: Arc<ExecutionRuntime>,
    reader_permits: Arc<tokio::sync::Semaphore>,
    refresh_after_delivery: Arc<RefreshAfterDelivery>,
    roots_refresh: Arc<Mutex<OfficialRootsRefreshState>>,
    #[cfg(test)]
    reader_probe: Option<Arc<TestReaderProbe>>,
}

/// Refresh registrations retained until rmcp confirms their response reached the transport.
///
/// Each registration owns the request's reader permit, so pending delivery state is bounded by the
/// configured concurrent-reader count rather than client request volume. Delivery, cancellation,
/// send failure, and transport shutdown all remove the registration and release that permit.
struct RefreshAfterDelivery {
    inner: Arc<Mutex<McpState>>,
    pending: Mutex<HashMap<rmcp::model::RequestId, tokio::sync::OwnedSemaphorePermit>>,
}

impl RefreshAfterDelivery {
    fn new(inner: Arc<Mutex<McpState>>) -> Self {
        Self {
            inner,
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn register(
        &self,
        request_id: rmcp::model::RequestId,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<(), String> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if pending.contains_key(&request_id) {
            return Err(format!(
                "MCP request id {request_id} already awaits response delivery"
            ));
        }
        pending.insert(request_id, permit);
        Ok(())
    }

    fn finish(&self, request_id: &rmcp::model::RequestId, delivered: bool) {
        let permit = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(request_id);
        let Some(permit) = permit else {
            return;
        };
        drop(permit);
        if !delivered {
            return;
        }
        match self.inner.lock() {
            Ok(mut server) => {
                let config = server.config.clone();
                server.refresh_worker.schedule(config);
            }
            Err(_) => {
                eprintln!(
                    "aise mcp serve: cannot schedule post-delivery index refresh because MCP state is poisoned"
                );
            }
        }
    }

    fn cancel(&self, request_id: &rmcp::model::RequestId) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(request_id);
    }

    fn cancel_all(&self) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }
}

struct RefreshAfterDeliveryTransport<T> {
    inner: T,
    refresh: Arc<RefreshAfterDelivery>,
}

impl<T> rmcp::transport::Transport<rmcp::RoleServer> for RefreshAfterDeliveryTransport<T>
where
    T: rmcp::transport::Transport<rmcp::RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let request_id = match &item {
            rmcp::model::ServerJsonRpcMessage::Response(response) => Some(response.id.clone()),
            rmcp::model::ServerJsonRpcMessage::Error(error) => error.id.clone(),
            _ => None,
        };
        let send = self.inner.send(item);
        let refresh = Arc::clone(&self.refresh);
        async move {
            let result = send.await;
            if let Some(request_id) = request_id {
                refresh.finish(&request_id, result.is_ok());
            }
            result
        }
    }

    fn receive(
        &mut self,
    ) -> impl Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>> + Send
    {
        let receive = self.inner.receive();
        let refresh = Arc::clone(&self.refresh);
        async move {
            let message = receive.await;
            if let Some(rmcp::model::ClientJsonRpcMessage::Notification(notification)) = &message {
                if let rmcp::model::ClientNotification::CancelledNotification(cancelled) =
                    &notification.notification
                {
                    if let Some(request_id) = &cancelled.params.request_id {
                        refresh.cancel(request_id);
                    }
                }
            }
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let close = self.inner.close();
        let refresh = Arc::clone(&self.refresh);
        async move {
            let result = close.await;
            refresh.cancel_all();
            result
        }
    }
}

/// Constant-memory single-flight state for the official SDK's deprecated roots compatibility API.
///
/// A new `Arc` is the generation token, avoiding integer wraparound. At most the current token and
/// the in-flight request's token survive, so notification storms retain `O(1)` state and issue at
/// most one `roots/list` request at a time.
struct OfficialRootsRefreshState {
    current_generation: Arc<()>,
    request_in_flight: bool,
}

impl Default for OfficialRootsRefreshState {
    fn default() -> Self {
        Self {
            current_generation: Arc::new(()),
            request_in_flight: false,
        }
    }
}

struct CancelBlockingWorkOnDrop {
    cancellation: Arc<QueryCancellation>,
    armed: bool,
}

impl CancelBlockingWorkOnDrop {
    fn new(cancellation: Arc<QueryCancellation>) -> Self {
        Self {
            cancellation,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelBlockingWorkOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

#[cfg(test)]
type TestRefreshRunner = Arc<dyn Fn(&Config, &AtomicBool) + Send + Sync>;

impl OfficialMcpServer {
    pub fn new(config: Config) -> anyhow::Result<Self> {
        let reader_bound = config.resolve_mcp_max_concurrent_reads()?;
        Self::with_reader_bound(config, reader_bound)
    }

    fn with_reader_bound(config: Config, reader_bound: NonZeroUsize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            reader_bound.get() <= tokio::sync::Semaphore::MAX_PERMITS,
            "MCP reader bound {} exceeds Tokio semaphore maximum {}",
            reader_bound,
            tokio::sync::Semaphore::MAX_PERMITS
        );
        let workers = NonZeroUsize::new(config.resolve_threads())
            .expect("Config::resolve_threads always returns at least one");
        let inner = Arc::new(Mutex::new(McpState::new(config)));
        Ok(Self {
            refresh_after_delivery: Arc::new(RefreshAfterDelivery::new(Arc::clone(&inner))),
            inner,
            reader_runtime: Arc::new(ExecutionRuntime::new(workers)),
            reader_permits: Arc::new(tokio::sync::Semaphore::new(reader_bound.get())),
            roots_refresh: Arc::new(Mutex::new(OfficialRootsRefreshState::default())),
            #[cfg(test)]
            reader_probe: None,
        })
    }

    async fn serve_transport<T, E, A>(
        self,
        transport: T,
    ) -> Result<
        rmcp::service::RunningService<rmcp::RoleServer, Self>,
        rmcp::service::ServerInitializeError,
    >
    where
        T: rmcp::transport::IntoTransport<rmcp::RoleServer, E, A>,
        E: std::error::Error + Send + Sync + 'static,
    {
        use rmcp::ServiceExt as _;

        let transport =
            rmcp::transport::IntoTransport::<rmcp::RoleServer, E, A>::into_transport(transport);
        let transport = RefreshAfterDeliveryTransport {
            inner: transport,
            refresh: Arc::clone(&self.refresh_after_delivery),
        };
        self.serve(transport).await
    }

    #[cfg(test)]
    fn with_reader_probe(mut self, probe: Arc<TestReaderProbe>) -> Self {
        self.reader_probe = Some(probe);
        self
    }

    #[cfg(test)]
    fn with_refresh_runner(self, runner: TestRefreshRunner) -> Self {
        self.inner.lock().unwrap().refresh_worker.runner = runner;
        self
    }
}

impl rmcp::ServerHandler for OfficialMcpServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(
            rmcp::model::Implementation::new("ai-session-search", env!("CARGO_PKG_VERSION"))
                .with_title("AI Session Search"),
        )
        .with_instructions(crate::integrations::agent_instructions())
    }

    fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::ListToolsResult, rmcp::ErrorData>> + Send + '_
    {
        let inner = Arc::clone(&self.inner);
        async move {
            let tools = tokio::task::spawn_blocking(move || {
                let mut server = inner
                    .lock()
                    .map_err(|_| "MCP state lock is poisoned".to_string())?;
                let tools = server.advertised_tools().clone();
                serde_json::from_value::<Vec<rmcp::model::Tool>>(tools)
                    .map_err(|error| format!("generated MCP tool catalogue is invalid: {error}"))
            })
            .await
            .map_err(|error| {
                rmcp::ErrorData::internal_error(
                    format!("MCP tool catalogue worker failed: {error}"),
                    None,
                )
            })?
            .map_err(|error| rmcp::ErrorData::internal_error(error, None))?;
            Ok(rmcp::model::ListToolsResult::with_all_items(tools))
        }
    }

    fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<rmcp::model::CallToolResult, rmcp::ErrorData>> + Send + '_
    {
        let inner = Arc::clone(&self.inner);
        let refresh_after_delivery = Arc::clone(&self.refresh_after_delivery);
        let runtime = Arc::clone(&self.reader_runtime);
        let permits = Arc::clone(&self.reader_permits);
        let request_cancellation = context.ct;
        let request_id = context.id;
        #[cfg(test)]
        let reader_probe = self.reader_probe.clone();
        #[cfg(test)]
        let admission_probe = reader_probe.clone();
        async move {
            #[cfg(test)]
            if let Some(probe) = &admission_probe {
                probe.record_admission_attempt();
            }
            let permit = tokio::select! {
                biased;
                _ = request_cancellation.cancelled() => {
                    #[cfg(test)]
                    if let Some(probe) = &admission_probe {
                        probe.record_cancelled_admission();
                    }
                    return Err(rmcp::ErrorData::internal_error(
                        "MCP tool call was cancelled while waiting for reader admission".to_string(),
                        None,
                    ));
                }
                permit = permits.acquire_owned() => permit.map_err(|_| {
                    rmcp::ErrorData::internal_error("MCP reader admission is closed".to_string(), None)
                })?,
            };
            let cancellation = Arc::new(QueryCancellation::new());
            let mut cancel_on_drop = CancelBlockingWorkOnDrop::new(Arc::clone(&cancellation));
            let worker_cancellation = Arc::clone(&cancellation);
            #[cfg(test)]
            let active_probe = reader_probe.clone();
            let mut worker = tokio::task::spawn_blocking(move || {
                (
                    execute_official_tool_call(
                        &inner,
                        &runtime,
                        request,
                        worker_cancellation,
                        #[cfg(test)]
                        reader_probe,
                    ),
                    permit,
                )
            });
            let result = tokio::select! {
                biased;
                _ = request_cancellation.cancelled() => {
                    #[cfg(test)]
                    if let Some(probe) = &active_probe {
                        probe.record_cancelled_active();
                    }
                    cancellation.cancel();
                    let _ = worker.await.map_err(|error| {
                        rmcp::ErrorData::internal_error(format!("cancelled MCP tool worker failed during cleanup: {error}"), None)
                    })?;
                    Err(rmcp::ErrorData::internal_error("MCP tool call was cancelled".to_string(), None))
                }
                result = &mut worker => {
                    result.map_err(|error| {
                        rmcp::ErrorData::internal_error(format!("MCP tool worker failed: {error}"), None)
                    })
                }
            };
            cancel_on_drop.disarm();
            let ((tool_result, refresh_after_call), permit) = result?;
            if refresh_after_call {
                refresh_after_delivery
                    .register(request_id, permit)
                    .map_err(|error| rmcp::ErrorData::internal_error(error, None))?;
            }
            Ok(tool_result)
        }
    }

    fn on_initialized(
        &self,
        context: rmcp::service::NotificationContext<rmcp::RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let inner = Arc::clone(&self.inner);
        let roots_refresh = Arc::clone(&self.roots_refresh);
        async move {
            match inner.lock() {
                Ok(mut server)
                    if server.config.index.refresh == crate::config::IndexRefresh::Auto =>
                {
                    let config = server.config.clone();
                    server.refresh_worker.schedule(config);
                }
                Ok(_) => {}
                Err(_) => {
                    eprintln!(
                        "aise mcp serve: cannot schedule startup index refresh because MCP state is poisoned"
                    );
                }
            }
            refresh_official_roots_authority(inner, roots_refresh, context.peer).await;
        }
    }

    fn on_roots_list_changed(
        &self,
        context: rmcp::service::NotificationContext<rmcp::RoleServer>,
    ) -> impl Future<Output = ()> + Send + '_ {
        let inner = Arc::clone(&self.inner);
        let roots_refresh = Arc::clone(&self.roots_refresh);
        async move {
            refresh_official_roots_authority(inner, roots_refresh, context.peer).await;
        }
    }
}

#[allow(deprecated)]
fn peer_supports_root_refresh(peer: &rmcp::service::Peer<rmcp::RoleServer>) -> bool {
    peer.peer_info()
        .is_some_and(|info| info.capabilities.roots.is_some())
}

async fn refresh_official_roots_authority(
    inner: Arc<Mutex<McpState>>,
    roots_refresh: Arc<Mutex<OfficialRootsRefreshState>>,
    peer: rmcp::service::Peer<rmcp::RoleServer>,
) {
    let restricted = inner.lock().is_ok_and(|server| {
        server.config.search.scope.mode == crate::config::SearchScopeMode::AllowedRoots
    });
    if !restricted || !peer_supports_root_refresh(&peer) {
        return;
    }

    let mut requested_generation = {
        let mut refresh = match roots_refresh.lock() {
            Ok(refresh) => refresh,
            Err(_) => {
                eprintln!(
                    "aise mcp serve: cannot refresh client roots because roots state is poisoned"
                );
                return;
            }
        };
        refresh.current_generation = Arc::new(());
        let generation = Arc::clone(&refresh.current_generation);
        match inner.lock() {
            Ok(mut server) => {
                // MCP roots are workspace coordination, not authentication. AI Session Search's
                // own allowed-roots policy treats the latest validated list as one authority input
                // and revokes it before requesting a replacement.
                server.app = None;
                server.harness_roots.clear();
                server.roots_error = None;
            }
            Err(_) => {
                eprintln!(
                    "aise mcp serve: cannot revoke stale client roots because MCP state is poisoned"
                );
                return;
            }
        }
        if refresh.request_in_flight {
            return;
        }
        refresh.request_in_flight = true;
        generation
    };

    loop {
        #[allow(deprecated)]
        let response = peer.list_roots().await;
        {
            let refresh = match roots_refresh.lock() {
                Ok(refresh) => refresh,
                Err(_) => {
                    eprintln!(
                        "aise mcp serve: cannot compare client root generations because roots state is poisoned"
                    );
                    return;
                }
            };
            if !Arc::ptr_eq(&refresh.current_generation, &requested_generation) {
                requested_generation = Arc::clone(&refresh.current_generation);
                continue;
            }
        }
        let config = match inner.lock() {
            Ok(server) => server.config.clone(),
            Err(_) => {
                eprintln!(
                    "aise mcp serve: cannot validate client roots because MCP state is poisoned"
                );
                return;
            }
        };
        let validated = response
            .map_err(|error| anyhow::anyhow!("roots/list failed: {error}"))
            .and_then(|result| parse_typed_mcp_roots(&result))
            .and_then(|roots| validate_mcp_roots(&config, roots))
            .map(|(roots, _)| roots);

        let mut refresh = match roots_refresh.lock() {
            Ok(refresh) => refresh,
            Err(_) => {
                eprintln!(
                    "aise mcp serve: cannot apply client roots because roots state is poisoned"
                );
                return;
            }
        };
        if !Arc::ptr_eq(&refresh.current_generation, &requested_generation) {
            requested_generation = Arc::clone(&refresh.current_generation);
            continue;
        }
        match inner.lock() {
            Ok(mut server) => {
                server.app = None;
                match validated {
                    Ok(roots) => {
                        server.harness_roots = roots;
                        server.roots_error = None;
                    }
                    Err(error) => {
                        server.harness_roots.clear();
                        server.roots_error = Some(mcp_roots_recovery_error(&error));
                    }
                }
            }
            Err(_) => {
                eprintln!(
                    "aise mcp serve: cannot apply client roots because MCP state is poisoned"
                );
            }
        }
        refresh.request_in_flight = false;
        return;
    }
}

fn execute_official_tool_call(
    inner: &Mutex<McpState>,
    runtime: &Arc<ExecutionRuntime>,
    request: rmcp::model::CallToolRequestParams,
    cancellation: Arc<QueryCancellation>,
    #[cfg(test)] reader_probe: Option<Arc<TestReaderProbe>>,
) -> (rmcp::model::CallToolResult, bool) {
    // This value exists only across preparation and immediate dispatch. Keeping it inline avoids a
    // heap allocation on every MCP read; its size is constant and independent of result volume.
    #[allow(clippy::large_enum_variant)]
    enum Preparation {
        /// The result, the configured delivery ceiling, and what a retry could ask for, all read
        /// under the same lock that produced the result, so the policy applied and the advice
        /// given both describe the call this actually was.
        Direct(Result<ToolResponse, String>, usize, ToolRecovery),
        Reader(Config, crate::search_scope::TrustedAccessInputs, bool),
    }

    let args = Value::Object(request.arguments.unwrap_or_default());
    let params = json!({ "name": request.name, "arguments": args });
    let prepared = (|| -> Result<Preparation, String> {
        if cancellation.is_cancelled() {
            return Err("MCP tool call was cancelled".to_string());
        }
        let mut server = inner
            .lock()
            .map_err(|_| "MCP state lock is poisoned".to_string())?;
        validate_tool_call(&params, server.advertised_tools())?;
        let recovery = ToolRecovery::for_call(
            params["name"].as_str().unwrap_or_default(),
            &args,
            server.advertised_tools(),
        );
        if let Some(error) = &server.roots_error {
            return Err(format!("invalid MCP roots authority: {error}"));
        }
        if is_schema_only_index_call(&params) {
            let ceiling = server.config.mcp.max_tool_result_chars;
            return Ok(Preparation::Direct(
                tool_query_session_index_cancellable(&args, &server.config, Some(&cancellation)),
                ceiling,
                recovery,
            ));
        }
        let tool_name = params["name"].as_str().unwrap_or_default();
        let mut config = server.config.clone();
        if tool_requests_existing_only(&params) {
            config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        } else {
            let app = server
                .open_app()
                .map_err(|error| format!("failed to prepare session index: {error:#}"))?;
            if config.index.refresh == crate::config::IndexRefresh::Auto {
                let readiness =
                    crate::background_refresh::readiness_status(app.config(), app.database())
                        .map_err(|error| {
                            format!("failed to read session index readiness: {error:#}")
                        })?;
                if readiness.snapshot.availability
                    == crate::models::IndexSnapshotAvailability::Unavailable
                    && tool_name != "get_index_status"
                {
                    return Ok(Preparation::Direct(
                        Ok(tool_not_ready_response(readiness)),
                        config.mcp.max_tool_result_chars,
                        recovery,
                    ));
                }
            } else {
                prepare_index_for_immediate_mcp_read(app)
                    .map_err(|error| format!("failed to prepare session index: {error:#}"))?;
            }
        }
        let inputs = mcp_access_inputs(&config, server.harness_roots.clone())
            .map_err(|error| format!("{error:#}"))?;
        let refresh_after_call = config.index.refresh == crate::config::IndexRefresh::Auto;
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        Ok(Preparation::Reader(config, inputs, refresh_after_call))
    })();

    let tool_name = params["name"].as_str().unwrap_or_default().to_owned();
    let (config, inputs, refresh_after_call) = match prepared {
        Ok(Preparation::Direct(result, ceiling, recovery)) => {
            return (
                match result {
                    Ok(response) => delivered_within_ceiling(
                        tool_response_to_rmcp(response),
                        &tool_name,
                        ceiling,
                        recovery,
                    ),
                    Err(error) => rmcp_tool_error(error),
                },
                false,
            );
        }
        Ok(Preparation::Reader(config, inputs, refresh_after_call)) => {
            (config, inputs, refresh_after_call)
        }
        Err(error) => return (rmcp_tool_error(error), false),
    };
    let app = match SessionSearch::open_prepared_reader(config, inputs, Arc::clone(runtime)) {
        Ok(app) => app,
        Err(error) => {
            return (
                rmcp_tool_error(format!("failed to open prepared session reader: {error:#}")),
                false,
            );
        }
    };
    app.database().install_query_cancellation(&cancellation);
    #[cfg(test)]
    let _reader_activity = reader_probe.as_ref().map(|probe| probe.enter());
    if cancellation.is_cancelled() {
        return (
            rmcp_tool_error("MCP tool call was cancelled".to_string()),
            false,
        );
    }
    #[cfg(test)]
    if let Some(probe) = &reader_probe {
        probe.record_dispatch();
    }
    let result = match dispatch_tool_cancellable(
        params["name"].as_str().unwrap_or_default(),
        &args,
        app.config(),
        app.database(),
        Some(&cancellation),
    ) {
        Ok(response) => delivered_within_ceiling(
            tool_response_to_rmcp(response),
            &tool_name,
            app.config().mcp.max_tool_result_chars,
            ToolRecovery::for_call(&tool_name, &args, &advertised_tools(app.config())),
        ),
        Err(error) => rmcp_tool_error(error),
    };
    (result, refresh_after_call)
}

#[cfg(test)]
struct TestReaderProbe {
    rendezvous: std::sync::Barrier,
    rendezvous_entries: usize,
    admission_attempts: std::sync::atomic::AtomicUsize,
    entries: std::sync::atomic::AtomicUsize,
    dispatches: std::sync::atomic::AtomicUsize,
    cancelled_admissions: std::sync::atomic::AtomicUsize,
    cancelled_active: std::sync::atomic::AtomicUsize,
    active: std::sync::atomic::AtomicUsize,
    max_active: std::sync::atomic::AtomicUsize,
}

#[cfg(test)]
impl TestReaderProbe {
    fn new(readers: usize) -> Self {
        Self {
            rendezvous: std::sync::Barrier::new(readers),
            rendezvous_entries: readers,
            admission_attempts: std::sync::atomic::AtomicUsize::new(0),
            entries: std::sync::atomic::AtomicUsize::new(0),
            dispatches: std::sync::atomic::AtomicUsize::new(0),
            cancelled_admissions: std::sync::atomic::AtomicUsize::new(0),
            cancelled_active: std::sync::atomic::AtomicUsize::new(0),
            active: std::sync::atomic::AtomicUsize::new(0),
            max_active: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn block_first_until_test() -> Self {
        Self {
            rendezvous: std::sync::Barrier::new(2),
            rendezvous_entries: 1,
            admission_attempts: std::sync::atomic::AtomicUsize::new(0),
            entries: std::sync::atomic::AtomicUsize::new(0),
            dispatches: std::sync::atomic::AtomicUsize::new(0),
            cancelled_admissions: std::sync::atomic::AtomicUsize::new(0),
            cancelled_active: std::sync::atomic::AtomicUsize::new(0),
            active: std::sync::atomic::AtomicUsize::new(0),
            max_active: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn enter(&self) -> TestReaderActivity<'_> {
        let entry = self.entries.fetch_add(1, Ordering::AcqRel) + 1;
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
        if entry <= self.rendezvous_entries {
            self.rendezvous.wait();
        }
        TestReaderActivity(self)
    }

    fn release_first(&self) {
        self.rendezvous.wait();
    }

    fn record_dispatch(&self) {
        self.dispatches.fetch_add(1, Ordering::AcqRel);
    }

    fn record_admission_attempt(&self) {
        self.admission_attempts.fetch_add(1, Ordering::AcqRel);
    }

    fn record_cancelled_admission(&self) {
        self.cancelled_admissions.fetch_add(1, Ordering::AcqRel);
    }

    fn record_cancelled_active(&self) {
        self.cancelled_active.fetch_add(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
struct TestReaderActivity<'a>(&'a TestReaderProbe);

#[cfg(test)]
impl Drop for TestReaderActivity<'_> {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn tool_response_to_rmcp(response: ToolResponse) -> rmcp::model::CallToolResult {
    let mut result =
        rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(response.text)]);
    result.structured_content = response.structured_content;
    result
}

/// What the call that overflowed can actually be retried with.
///
/// The over-ceiling error is the only thing a caller receives, so every control it names has to
/// be one the failed tool accepts. Five of the eight tools page and three do not; telling a
/// `get_index_status` caller to lower `limit` and continue from `page.next_offset` names two
/// arguments its schema rejects and one response field it never returns, which spends the
/// caller's next round trip proving the advice was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolRecovery {
    /// How this call was paged. An omitted `limit` is `Page` of the configured default rather
    /// than an absence, because that resolved value is the number a smaller retry has to beat.
    paging: ToolPaging,
    /// The per-result reductions this tool advertises, so the advice names none it rejects.
    reducers: ReducingArguments,
}

/// How the failed call was paged, which decides what a smaller retry can even be.
///
/// Three states, not two. Collapsing the middle one into `Unpaged` is what told an installed
/// Codex run that `search_sessions` had "no smaller page to ask for", moments before it answered
/// the same question with `limit=20`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolPaging {
    /// The tool returns one whole answer and advertises no `limit`, so naming one would send the
    /// caller to an argument its own schema rejects.
    Unpaged,
    /// The caller asked for every match with `limit=0`. There is no requested page to halve, but
    /// the tool does page, so a finite retry exists and is sized from what came back.
    EveryMatch,
    /// The call resolved to this page, explicitly or from the advertised default.
    Page(usize),
}

impl ToolRecovery {
    /// Read the page a call resolved to from the same advertised schema the caller read.
    ///
    /// The schema already carries each pageable tool's configured default in its `limit`
    /// property, put there so the model can see it (`WP-N-generate-message-search-descriptions`).
    /// Deriving from it keeps one owner: a tool that gains or loses pagination changes its schema
    /// and this follows, where a hand-listed table would keep answering for the old surface.
    fn for_call(tool_name: &str, args: &Value, advertised: &Value) -> Self {
        let properties = advertised
            .as_array()
            .and_then(|tools| tools.iter().find(|tool| tool["name"] == tool_name))
            .and_then(|tool| tool.get("inputSchema"))
            .and_then(|schema| schema.get("properties"));
        let accepts = |argument: &str| {
            properties.is_some_and(|properties| properties.get(argument).is_some())
        };
        // Which of the "ask for less of each result" arguments this particular tool has. Read from
        // the same schema rather than assumed, for the same reason `limit` is: three of the four
        // pageable tools accept none of them, and naming one anyway hands the caller an argument
        // its own schema rejects.
        let reducers = ReducingArguments {
            context: accepts("context"),
            detail: accepts("detail"),
            receipt_level: accepts("receipt_level"),
            lines_per_message: accepts("lines_per_message"),
            field_view: accepts("field_view"),
        };
        let Some(limit_property) = properties.and_then(|properties| properties.get("limit")) else {
            return Self {
                paging: ToolPaging::Unpaged,
                reducers,
            };
        };
        // An explicit argument wins, then the default the schema advertises. `limit=0` means
        // "every match" on the session tools: unbounded, so there is no requested page to halve,
        // but the tool still pages and a finite retry is still available.
        let explicit = args.get("limit").and_then(Value::as_u64);
        let paging =
            match explicit.or_else(|| limit_property.get("default").and_then(Value::as_u64)) {
                Some(0) | None => ToolPaging::EveryMatch,
                Some(page) => usize::try_from(page)
                    .map(ToolPaging::Page)
                    .unwrap_or(ToolPaging::EveryMatch),
            };
        Self { paging, reducers }
    }
}

/// Which "return less of each result" arguments the failed tool actually advertises.
///
/// The advice used to name `context`, `detail` and `receipt_level` on every pageable tool. Only
/// `search_messages` accepts all three; `search_sessions`, `list_sessions` and
/// `query_session_index` accept none, so their callers were told to pass three arguments those
/// schemas reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ReducingArguments {
    context: bool,
    detail: bool,
    receipt_level: bool,
    lines_per_message: bool,
    field_view: bool,
}

impl ReducingArguments {
    /// The remedies this tool can actually act on, as clauses, most effective first.
    fn clauses(self) -> Vec<&'static str> {
        let mut clauses = Vec::new();
        if self.context {
            clauses.push("lower context");
        }
        if self.detail {
            clauses.push("set detail=compact");
        }
        if self.receipt_level {
            clauses.push("set receipt_level=none");
        }
        // One clause for the pair, because "narrow the selected field with X, or narrow the
        // selected field with Y" says the same thing twice.
        match (self.lines_per_message, self.field_view) {
            (true, true) => {
                clauses.push("narrow the selected field with lines_per_message or field_view")
            }
            (true, false) => clauses.push("narrow the selected field with lines_per_message"),
            (false, true) => clauses.push("narrow the selected field with field_view"),
            (false, false) => {}
        }
        clauses
    }

    /// Those clauses as one sentence fragment, or nothing when the tool has no such argument.
    fn sentence(self) -> Option<String> {
        let clauses = self.clauses();
        if clauses.is_empty() {
            return None;
        }
        Some(clauses.join(", or "))
    }
}

/// Return the complete result, or one error saying how to get it. Never a partial success.
///
/// Codex middle-truncates an oversized result with no marker, keeping a plausible head and tail,
/// so a caller cannot tell a complete answer from a mutilated one. The obvious defence is to trim
/// the response until it fits. `REQ008-reject-hidden-cutoffs` forbids exactly that -- "silent
/// aggregate byte ceilings, row ceilings, content truncation, time limits, or fallback pages" --
/// and its own resolution for `max_hits_per_page` is the pattern followed here: an oversized
/// resolved request errors, it does not silently clamp.
///
/// So nothing is removed. Results, context neighbours, evidence, detail, include groups and
/// receipts are exactly what the caller asked for or the call fails, because
/// `REQ004-separate-retrieval-presentation` puts result count and context membership outside what
/// presentation may alter, and `REQ005-return-match-evidence` puts match evidence below any
/// floor. A caller who asked for more than fits gets told so and told what fits.
///
/// The error carries all five parts `REQ047-return-actionable-recovery` requires, and that
/// requirement's own note that "preserving actionability takes priority over shaving a few
/// diagnostic bytes" is why it is long on purpose. Part three is the one that would be expensive
/// to omit: a caller who does not know the page was already selected may re-scope the query
/// instead of just lowering `limit`, and `REQ007-preserve-page-identity` is what makes the
/// promise that a retry returns the same page true.
fn enforce_tool_result_ceiling(
    result: rmcp::model::CallToolResult,
    tool_name: &str,
    ceiling: usize,
    recovery: ToolRecovery,
) -> Result<rmcp::model::CallToolResult, String> {
    // An error result is already the small path, and refusing to deliver an error because the
    // error is large would leave the caller with nothing at all to act on.
    if result.is_error.unwrap_or(false) {
        return Ok(result);
    }
    let measured = match serde_json::to_string(&result) {
        Ok(serialized) => serialized.chars().count(),
        // Measuring is not the operation; a result that cannot be measured is still a result,
        // and failing the call over the size check would turn a diagnostic into an outage.
        Err(_) => return Ok(result),
    };
    if measured <= ceiling {
        return Ok(result);
    }
    // What a smaller page can remove is what the page actually returned, not what it asked for.
    // A call that requested 20 and matched 3 is three results wide, so suggesting 15 changes
    // nothing and the retry overflows again on a byte-identical response. The count comes from
    // the response's own `page.returned`, the field the advice already tells callers to check.
    let returned = result.structured_content.as_ref().and_then(returned_count);
    let paging_paths = PagingPaths::of(result.structured_content.as_ref());
    let effective_page = match (recovery.paging, returned) {
        (ToolPaging::Unpaged, _) => None,
        // Nothing was requested to shrink from, so the page that overflowed is the one the
        // response actually carries. Without this the caller is told no finite page exists.
        (ToolPaging::EveryMatch, returned) => returned,
        (ToolPaging::Page(requested), Some(returned)) => Some(requested.min(returned)),
        (ToolPaging::Page(requested), None) => Some(requested),
    };
    // Split the response into the part a smaller page shrinks and the part it does not.
    //
    // Three things scale with the page and all three have to be counted, because missing one
    // moves its bytes into the fixed side and makes a retry look impossible. `results` is the
    // obvious one. `included` carries one entry per distinct session in the page, so it shrinks
    // too. And the `CallToolResult` carries a text rendering of the same results beside the
    // structured content, which on a measured 20-result response was 4,200 of its 31,483
    // characters -- counting that as fixed put the envelope over the ceiling and produced advice
    // saying no page could ever fit, on a response whose 2-result page measured 4,930.
    let shape = result.structured_content.as_ref().map(|structured| {
        let rendered = serde_json::to_string(&result.content)
            .map(|serialized| serialized.chars().count())
            .unwrap_or_default();
        // Each result's own size, in order, so a shorter page is priced by the results it would
        // actually keep rather than by the page mean, which is computed partly from results it
        // would drop.
        let items = returned_items(structured, returned);
        let result_sizes: Vec<usize> = items
            .map(|items| {
                items
                    .iter()
                    .map(|hit| {
                        serde_json::to_string(hit)
                            .map(|serialized| serialized.chars().count())
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        // `included` is deliberately not counted as scaling. It carries one entry per distinct
        // session, not per result, and a shorter page usually spans the same sessions: a measured
        // 20-result page and its 3-result prefix both covered exactly two. Treating it as
        // shrinking made the estimate 26% optimistic, which is the difference between advice that
        // works and advice that fails on the caller's next call.
        let scaling = result_sizes.iter().sum::<usize>() + rendered;
        ResponseShape {
            scaling_chars: scaling,
            envelope_chars: measured.saturating_sub(scaling),
            result_sizes,
        }
    });

    // The preamble says what did not happen, and says it in terms every tool has. An earlier
    // wording enumerated "results, context neighbours, match evidence, include groups or
    // receipts", which are message-search concepts: on `search_sessions` that promises the
    // survival of four things the tool never had.
    Err(format!(
        "{tool_name} produced {measured} characters, above the configured MCP ceiling of \
         {ceiling}. Nothing was returned and nothing was written or cached, and no part of the \
         answer was silently removed to make it fit, because a trimmed answer would answer a \
         different question than the one asked. {}",
        recovery_advice(
            tool_name,
            measured,
            ceiling,
            effective_page,
            shape.as_ref(),
            recovery.reducers,
            paging_paths,
        )
    ))
}

/// Where this tool reports its paging, so the advice quotes a field the caller will actually find.
///
/// Message search nests them under `page`; the session tools put them at the top level. Naming
/// `page.next_offset` to a `search_sessions` caller sends them looking for an object that response
/// does not contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PagingPaths {
    returned: &'static str,
    has_more: &'static str,
    next_offset: &'static str,
}

impl PagingPaths {
    const NESTED: Self = Self {
        returned: "page.returned",
        has_more: "page.has_more",
        next_offset: "page.next_offset",
    };
    const TOP_LEVEL: Self = Self {
        returned: "returned",
        has_more: "has_more",
        next_offset: "next_offset",
    };

    fn of(structured: Option<&Value>) -> Self {
        match structured {
            Some(structured) if structured.get("page").is_some() => Self::NESTED,
            _ => Self::TOP_LEVEL,
        }
    }
}

/// How many items the response says it returned, whichever way this tool says it.
///
/// Message search reports `page.returned`; the session tools report `returned` at the top level.
/// Reading one fixed path finds the count on some tools and silently reports none on the others,
/// and "no count" is what makes a retry look impossible.
fn returned_count(structured: &Value) -> Option<usize> {
    structured
        .get("page")
        .and_then(|page| page.get("returned"))
        .or_else(|| structured.get("returned"))
        .and_then(Value::as_u64)
        .and_then(|returned| usize::try_from(returned).ok())
}

/// The items a smaller page would return fewer of, found by the count the response reports.
///
/// The array is named `results` on message search and `sessions` on the session tools, and a
/// hand-listed table of those names is one more thing to keep in step with the tools. The response
/// already says how many items it returned, so the array that *is* those items is the top-level
/// array of that length -- and where more than one matches, the largest, since a page shrinks the
/// payload rather than a same-length index beside it.
fn returned_items(structured: &Value, returned: Option<usize>) -> Option<&Vec<Value>> {
    let returned = returned?;
    structured
        .as_object()?
        .values()
        .filter_map(Value::as_array)
        .filter(|items| items.len() == returned)
        .max_by_key(|items| {
            serde_json::to_string(items)
                .map(|serialized| serialized.len())
                .unwrap_or_default()
        })
}

/// What one over-ceiling response is made of, measured rather than assumed.
///
/// Response size is affine in the page, not proportional: a fixed envelope plus a per-result
/// cost. Scaling the page by `ceiling / measured` ignores the envelope and therefore overshoots
/// whenever the envelope is a real share of the ceiling. An installed search measuring 29,956
/// characters at `limit=20` against a 6,000 ceiling was told to retry at 3, and 3 measured 6,358:
/// the advice failed on its own next call. Both quantities are in hand at the moment the error is
/// built, so they are measured instead.
#[derive(Debug, Clone)]
struct ResponseShape {
    /// Everything a smaller page shrinks: the results, the per-session metadata beside them, and
    /// the text rendering of both.
    scaling_chars: usize,
    /// Everything it does not: the echoed request, the page block, the schema version, and the
    /// protocol wrapper.
    envelope_chars: usize,
    /// Each result's own serialized size, in the order they were returned.
    result_sizes: Vec<usize>,
}

impl ResponseShape {
    /// The largest page that fits, priced by the results that page would actually keep.
    ///
    /// A shorter page keeps a prefix of these results, so the prefix is what it costs. The
    /// per-result metadata and text rendering shrink with it, and they are charged in proportion
    /// to the result bytes retained -- both are derived from the same results, so they scale with
    /// them rather than with the count.
    fn largest_page_within(&self, ceiling: usize) -> Option<usize> {
        /// Aim below the ceiling rather than at it.
        ///
        /// This is an estimate built from one sample, and the parts it cannot see all push the
        /// same way. The text rendering carries fixed structure that does not shrink with the
        /// page, and a shorter page can span the same sessions as a longer one, so the pieces
        /// charged proportionally are charged slightly too little. On the measured response the
        /// remaining error was about 8%, all of it optimistic.
        ///
        /// Aiming at the ceiling therefore lands just over it, which is the one outcome that
        /// makes the advice worthless: the caller follows it exactly and gets the same error. The
        /// cost of aiming low is one more page than strictly necessary; the cost of aiming high
        /// is a failed call and a caller who stops trusting the next suggestion.
        const TARGET_SHARE: f64 = 0.85;

        let results_only: usize = self.result_sizes.iter().sum();
        if results_only == 0 {
            return None;
        }
        let target = (ceiling as f64 * TARGET_SHARE) as usize;
        // What each retained result drags along with it, beyond its own bytes: its share of the
        // text rendering of the same results.
        let companion_ratio = self.scaling_chars as f64 / results_only as f64;
        let mut retained = 0usize;
        let mut fitting = 0usize;
        for (index, size) in self.result_sizes.iter().enumerate() {
            retained += size;
            let projected =
                self.envelope_chars + (retained as f64 * companion_ratio).ceil() as usize;
            if projected > target {
                break;
            }
            fitting = index + 1;
        }
        (fitting >= 1).then_some(fitting)
    }
}

/// What to do about it, naming only controls the failed tool accepts.
fn recovery_advice(
    tool_name: &str,
    measured: usize,
    ceiling: usize,
    effective_page: Option<usize>,
    shape: Option<&ResponseShape>,
    reducers: ReducingArguments,
    paging_paths: PagingPaths,
) -> String {
    let raise_the_ceiling = "Or raise the ceiling for this deployment with [mcp] \
         max_tool_result_chars in config.toml, or AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS in this \
         server's registration env block.";
    // Named only where the tool has them. Three of the four pageable tools accept none of these,
    // so an unconditional list is three rejected arguments dressed as a remedy.
    let reduce_each_result = reducers.sentence();

    let Some(resolved_page) = effective_page else {
        // A tool that does not page cannot be told to page. Naming `limit` here would send the
        // caller to an argument its own schema rejects, and `page.next_offset` to a field it
        // never returns.
        let instead = match &reduce_each_result {
            Some(clauses) => format!("Ask for less of it instead: {clauses}."),
            None => "Request less of it with the arguments this tool does accept, or call a \
                     narrower tool."
                .to_owned(),
        };
        return format!(
            "{tool_name} returns one whole answer rather than a page, so there is no smaller \
             page to ask for. {instead} {raise_the_ceiling}"
        );
    };

    match suggested_retry_limit(measured, ceiling, resolved_page, shape) {
        Some(suggested) => {
            let also = match &reduce_each_result {
                Some(clauses) => format!(", or {clauses}"),
                None => String::new(),
            };
            format!(
                "The retrieval plan and the selected page are unchanged, so a retry at the same \
                 offset returns the same results and you do not need to re-scope the query. Retry \
                 with limit={suggested} at the same offset{also}. {raise_the_ceiling} The retry \
                 succeeded when the response carries {} and {}; continue from {}.",
                paging_paths.returned, paging_paths.has_more, paging_paths.next_offset
            )
        }
        // No smaller page fits, either because one result already exceeds the ceiling or because
        // the part of the response a smaller page cannot remove already does. Naming a limit here
        // would be advice that fails on the caller's next call.
        None => {
            let why = match shape {
                Some(shape) if shape.envelope_chars >= ceiling => format!(
                    "The {} characters this response carries outside its results already exceed \
                     the ceiling, so no page size can fit",
                    shape.envelope_chars
                ),
                _ => "A single result exceeds the ceiling on its own, so no smaller page can fit"
                    .to_owned(),
            };
            let instead = match &reduce_each_result {
                Some(clauses) => format!("Reduce what each result carries instead: {clauses}."),
                None => "This tool has no argument that returns less of each result, so narrow \
                         the query itself or call a narrower tool."
                    .to_owned(),
            };
            format!("{why}. {instead} {raise_the_ceiling}")
        }
    }
}

/// The `limit` to suggest, solved from what this response is actually made of.
///
/// A suggestion that errors again costs the caller a round trip and teaches them the advice is
/// unreliable, so it is worth solving properly rather than scaling and hoping.
///
/// Response size is affine in the page: a fixed envelope -- the echoed request, the page block,
/// the deduplicated session metadata -- plus a per-result cost. A smaller page removes results
/// and leaves the envelope, so the fitting page is `(ceiling - envelope) / per_result`, not
/// `page * ceiling / measured`. Both terms are measured from the response in hand rather than
/// assumed, which is what makes this exact instead of approximate.
///
/// Two earlier versions of this function each shipped a defect that a caller met before a test
/// did. The first scaled from a hard-coded 15, so a `limit=1` overflow was told to retry with
/// `limit=12`: more work than the call that had just failed. The second scaled proportionally
/// with a 0.85 factor meant to absorb the envelope, and an installed search that measured 29,956
/// characters at `limit=20` against a 6,000 ceiling was told to retry at 3, which measured 6,358
/// and failed again. Proportional scaling cannot absorb an envelope whose share of the ceiling is
/// unknown, and on that response the envelope alone was over a third of it.
///
/// `None` means no page fits: either a single result exceeds the ceiling, or the envelope does.
/// Both are real states, and naming a limit in either would be advice that cannot be followed.
fn suggested_retry_limit(
    measured: usize,
    ceiling: usize,
    resolved_page: usize,
    shape: Option<&ResponseShape>,
) -> Option<usize> {
    if resolved_page <= 1 {
        return None;
    }
    let Some(shape) = shape else {
        // No structured content to decompose, so fall back to proportional scaling with room for
        // an envelope of unknown size. Deliberately pessimistic: a suggestion smaller than it
        // needed to be costs one extra page, and one that is too large costs the caller a failed
        // call and their confidence in every later suggestion.
        const UNKNOWN_SHAPE_FACTOR: f64 = 0.5;
        let scaled =
            (ceiling as f64 / measured as f64) * UNKNOWN_SHAPE_FACTOR * resolved_page as f64;
        return Some((scaled.floor() as usize).clamp(1, resolved_page - 1));
    };
    // Nothing a smaller page can do reduces the envelope, so if it alone is over the ceiling no
    // page size helps and saying otherwise wastes the caller's next call.
    if shape.envelope_chars >= ceiling {
        return None;
    }
    shape
        .largest_page_within(ceiling)
        .map(|fitting| fitting.min(resolved_page - 1))
        .filter(|fitting| *fitting >= 1)
}

/// Deliver the result, or the actionable error explaining what fits instead.
fn delivered_within_ceiling(
    result: rmcp::model::CallToolResult,
    tool_name: &str,
    ceiling: usize,
    recovery: ToolRecovery,
) -> rmcp::model::CallToolResult {
    match enforce_tool_result_ceiling(result, tool_name, ceiling, recovery) {
        Ok(delivered) => delivered,
        Err(error) => rmcp_tool_error(error),
    }
}

fn rmcp_tool_error(error: String) -> rmcp::model::CallToolResult {
    rmcp::model::CallToolResult::error(vec![rmcp::model::ContentBlock::text(error)])
}

impl McpState {
    fn new(config: Config) -> Self {
        Self {
            config,
            app: None,
            harness_roots: Vec::new(),
            roots_error: None,
            advertised_tools: None,
            refresh_worker: RefreshWorker::default(),
        }
    }

    fn advertised_tools(&mut self) -> &Value {
        let config = &self.config;
        self.advertised_tools
            .get_or_insert_with(|| advertised_tools(config))
    }

    fn open_app(&mut self) -> anyhow::Result<&SessionSearch> {
        if let Some(error) = &self.roots_error {
            anyhow::bail!("invalid MCP roots authority: {error}");
        }
        open_mcp_app(&mut self.app, &self.config, &self.harness_roots)
    }
}

fn tool_requests_existing_only(params: &Value) -> bool {
    params
        .get("arguments")
        .and_then(|arguments| arguments.get("index_refresh"))
        .and_then(Value::as_str)
        == Some("existing-only")
}

fn validate_tool_call(params: &Value, tools: &Value) -> Result<(), String> {
    let params = params
        .as_object()
        .ok_or_else(|| "tools/call params must be an object".to_string())?;
    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call name must be a string".to_string())?;
    let tool = tools
        .as_array()
        .and_then(|tools| tools.iter().find(|tool| tool["name"] == tool_name))
        .ok_or_else(|| {
            // Name the likeliest intended tool and every tool this server actually serves. A
            // caller that mistyped or guessed can correct it from this message alone, without a
            // second tools/list call.
            unknown_tool_message(tool_name, tools)
        })?;
    let arguments = params.get("arguments").unwrap_or(&Value::Null);
    let empty_arguments = json!({});
    let arguments = if arguments.is_null() && !params.contains_key("arguments") {
        &empty_arguments
    } else {
        arguments
    };
    let schema = &tool["inputSchema"];
    validate_schema_value(arguments, schema, schema, tool_name, "arguments")
}

fn validate_schema_value(
    value: &Value,
    schema: &Value,
    root: &Value,
    tool_name: &str,
    path: &str,
) -> Result<(), String> {
    let invalid = |detail: String| format!("invalid {tool_name} {path}: {detail}");
    // Resolve a local pointer before doing anything else. A `$ref` branch carries no `type`,
    // no `properties` and no `required`, so a validator that walks past it accepts every value
    // that reaches it -- which turns naming a shared shape into silently widening what the
    // parameter accepts. MCP forbids dereferencing a network URI, so only `#/` pointers resolve
    // and anything else is rejected rather than treated as permissive.
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let Some(pointer) = reference.strip_prefix('#') else {
            return Err(invalid(format!(
                "schema references {reference}, which is not a local pointer"
            )));
        };
        let Some(resolved) = root.pointer(pointer) else {
            return Err(invalid(format!(
                "schema references {reference}, which does not resolve"
            )));
        };
        return validate_schema_value(value, resolved, root, tool_name, path);
    }
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| validate_schema_value(value, branch, root, tool_name, path).is_ok())
            .count();
        if matching != 1 {
            return Err(invalid(format!(
                "must match exactly one schema alternative, matched {matching}"
            )));
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid(type_mismatch("object", value)))?;
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for key in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(key) {
                        return Err(invalid(format!("missing required parameter '{key}'")));
                    }
                }
            }
            let properties = schema.get("properties").and_then(Value::as_object);
            if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
                for key in object.keys() {
                    if !properties.is_some_and(|properties| properties.contains_key(key)) {
                        let accepted: Vec<&str> = properties
                            .map(|properties| properties.keys().map(String::as_str).collect())
                            .unwrap_or_default();
                        return Err(format!(
                            "unknown {tool_name} parameter at {path}: {key}{}",
                            unknown_key_hint(key, &accepted)
                        ));
                    }
                }
            }
            if let Some(properties) = properties {
                for (key, child) in object {
                    if let Some(child_schema) = properties.get(key) {
                        validate_schema_value(
                            child,
                            child_schema,
                            root,
                            tool_name,
                            &format!("{path}/{key}"),
                        )?;
                    }
                }
            }
        }
        Some("array") => {
            let array = value
                .as_array()
                .ok_or_else(|| invalid(type_mismatch("array", value)))?;
            if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
                if array.len() < minimum as usize {
                    return Err(invalid(format!("expected at least {minimum} items")));
                }
            }
            if schema.get("uniqueItems") == Some(&Value::Bool(true)) {
                for (index, item) in array.iter().enumerate() {
                    if array[..index].contains(item) {
                        return Err(invalid(format!(
                            "item {index} duplicates an earlier array value"
                        )));
                    }
                }
            }
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in array.iter().enumerate() {
                    validate_schema_value(
                        item,
                        item_schema,
                        root,
                        tool_name,
                        &format!("{path}/{index}"),
                    )?;
                }
            }
        }
        Some("string") if !value.is_string() => {
            return Err(invalid(type_mismatch("string", value)));
        }
        Some("boolean") if !value.is_boolean() => {
            return Err(invalid(type_mismatch("boolean", value)));
        }
        Some("integer") if value.as_i64().is_none() && value.as_u64().is_none() => {
            return Err(invalid(type_mismatch("integer", value)));
        }
        Some("number") if !value.is_number() => {
            return Err(invalid(type_mismatch("number", value)));
        }
        Some(_) | None => {}
    }
    if let (Some(text), Some(minimum)) = (
        value.as_str(),
        schema.get("minLength").and_then(Value::as_u64),
    ) {
        if text.chars().count() < minimum as usize {
            return Err(invalid(format!(
                "must contain at least {minimum} Unicode characters"
            )));
        }
    }
    if let (Some(actual), Some(minimum)) = (
        value.as_f64(),
        schema.get("minimum").and_then(Value::as_f64),
    ) {
        if actual < minimum {
            // Append the parameter's own description: for paging, `0` is a documented
            // selection rather than the floor, so the bound alone leaves the caller without a
            // replacement value. Reusing the authored text keeps one source of truth.
            let guidance = schema
                .get("description")
                .and_then(Value::as_str)
                .map(|description| format!(" — {description}"))
                .unwrap_or_default();
            return Err(invalid(format!("must be at least {minimum}{guidance}")));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let choices = allowed
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(invalid(format!("must be one of {choices}, got {value}")));
        }
    }
    Ok(())
}

/// Format a type mismatch. `null` gets a corrective hint because several MCP clients
/// serialize unset optionals as explicit `null`, which this server deliberately rejects
/// rather than treating as omitted.
fn type_mismatch(expected: &str, value: &Value) -> String {
    let got = json_type(value);
    if value.is_null() {
        format!("expected {expected}, got null; omit the parameter to use its default")
    } else {
        format!("expected {expected}, got {got}")
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn open_mcp_app<'a>(
    slot: &'a mut Option<SessionSearch>,
    config: &Config,
    harness_roots: &[std::path::PathBuf],
) -> anyhow::Result<&'a SessionSearch> {
    if slot.is_none() {
        *slot = Some(SessionSearch::open_with_access_inputs(
            config.clone(),
            mcp_access_inputs(config, harness_roots.to_vec())?,
        )?);
    }
    Ok(slot.as_ref().expect("application slot initialized above"))
}

fn mcp_access_inputs(
    config: &Config,
    harness_roots: Vec<std::path::PathBuf>,
) -> anyhow::Result<crate::search_scope::TrustedAccessInputs> {
    crate::search_scope::TrustedAccessInputs::capture(&config.search.scope, harness_roots)
}

fn parse_mcp_root_uri(uri: &str, index: usize) -> anyhow::Result<std::path::PathBuf> {
    let url = url::Url::parse(uri)
        .map_err(|error| anyhow::anyhow!("roots[{index}].uri is invalid: {error}"))?;
    if url.scheme() != "file" {
        anyhow::bail!("roots[{index}].uri must use the file scheme");
    }
    url.to_file_path()
        .map_err(|_| anyhow::anyhow!("roots[{index}].uri is not a local file path"))
}

#[allow(deprecated)]
fn parse_typed_mcp_roots(
    result: &rmcp::model::ListRootsResult,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    result
        .roots
        .iter()
        .enumerate()
        .map(|(index, root)| parse_mcp_root_uri(&root.uri, index))
        .collect()
}

fn validate_mcp_roots(
    config: &Config,
    roots: Vec<std::path::PathBuf>,
) -> anyhow::Result<(
    Vec<std::path::PathBuf>,
    crate::search_scope::TrustedAccessInputs,
)> {
    let inputs = mcp_access_inputs(config, roots.clone())?;
    crate::search_scope::EffectiveAccessScope::resolve(&config.search.scope, inputs.clone())?;
    Ok((roots, inputs))
}

fn mcp_roots_recovery_error(error: &anyhow::Error) -> String {
    format!(
        "{error:#}; return existing local file:// directory URIs from roots/list, then send notifications/roots/list_changed"
    )
}

fn prepare_index_for_immediate_mcp_read(app: &SessionSearch) -> anyhow::Result<()> {
    let outcome = crate::indexer::prepare_index_for_read_now(app.config(), app.database());
    match outcome {
        Ok(None)
        | Ok(Some(crate::indexer::AutoReindexOutcome::Updated { .. }))
        | Ok(Some(crate::indexer::AutoReindexOutcome::SkippedFresh)) => Ok(()),
        Ok(Some(crate::indexer::AutoReindexOutcome::SkippedBusy)) => {
            if !app.database().has_sessions()? {
                anyhow::bail!(
                    "initial session indexing is running in another process, so results would be incomplete; retry this tool call after that writer finishes. Use get_index_status after the active writer releases the index to verify readiness"
                );
            }
            eprintln!(
                "aise mcp serve: auto-reindex skipped because another process is writing; serving existing index"
            );
            Ok(())
        }
        Ok(Some(crate::indexer::AutoReindexOutcome::SkippedLockUnavailable { reason })) => {
            if !app.database().has_sessions()? {
                anyhow::bail!(
                    "initial session indexing cannot acquire its update lock, so results would be incomplete ({reason}); fix the reported lock-path problem, then retry this tool call or run `aise reindex`"
                );
            }
            eprintln!(
                "aise mcp serve: auto-reindex skipped because the update lock is unavailable; serving existing index ({reason})"
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}

struct RefreshWorker {
    sender: Option<mpsc::SyncSender<()>>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    #[cfg(test)]
    runner: TestRefreshRunner,
}

impl Default for RefreshWorker {
    fn default() -> Self {
        Self {
            sender: None,
            cancel: Arc::new(AtomicBool::new(false)),
            handle: None,
            #[cfg(test)]
            runner: Arc::new(run_background_refresh),
        }
    }
}

impl RefreshWorker {
    fn schedule(&mut self, config: Config) {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            self.sender = None;
        }
        if self.handle.is_none() {
            self.cancel.store(false, Ordering::Release);
            let cancel = Arc::clone(&self.cancel);
            let (sender, receiver) = mpsc::sync_channel(1);
            #[cfg(test)]
            let runner = Arc::clone(&self.runner);
            self.handle = Some(thread::spawn(move || {
                while receiver.recv().is_ok() {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    #[cfg(not(test))]
                    run_background_refresh(&config, &cancel);
                    #[cfg(test)]
                    runner(&config, &cancel);
                    // The capacity-one channel coalesces an arbitrary burst into one trailing run.
                    // Do not drain it here: that one retained signal may describe a source change
                    // that occurred after the active reconciliation passed its discovery point.
                }
            }));
            self.sender = Some(sender);
        }
        if let Some(sender) = &self.sender {
            match sender.try_send(()) {
                Ok(()) | Err(mpsc::TrySendError::Full(())) => {}
                Err(mpsc::TrySendError::Disconnected(())) => {
                    self.sender = None;
                }
            }
        }
    }
}

impl Drop for RefreshWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.sender.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_background_refresh(config: &Config, cancel: &AtomicBool) {
    if cancel.load(Ordering::Acquire) {
        return;
    }
    if let Err(error) = crate::background_refresh::run(
        config,
        crate::background_refresh::BackgroundRefreshOrigin::Mcp,
        &|| cancel.load(Ordering::Acquire),
    ) {
        eprintln!("aise mcp serve: background index refresh failed: {error:#}");
    }
}

fn provider_filter_schema(provider_values: &[&str], description: &str) -> Value {
    json!({
        "type": "string",
        "enum": provider_values,
        "description": description,
    })
}

fn provider_set_schema(provider_values: &[&str], description: &str) -> Value {
    json!({
        "type": "array",
        "minItems": 1,
        "uniqueItems": true,
        "items": {
            "type": "string",
            "enum": provider_values
        },
        "description": description
    })
}

/// Tool annotations shared by every tool this server exposes. The requested operations retrieve
/// local results and never mutate provider transcripts or user-authored configuration. In `auto`
/// refresh mode, search preparation or the server lifecycle may maintain the derived index;
/// `existing-only` instead opens SQLite read-only and forbids that maintenance. `readOnlyHint`
/// describes the caller-visible operation, while `openWorldHint: false` states that results come
/// from the closed local index rather than an external service.
fn read_only_tool_annotations() -> Value {
    json!({
        "readOnlyHint": true,
        "openWorldHint": false,
    })
}

fn get_session_output_schema() -> Value {
    // Three shapes are named. `MessageReference` and `ParsedReference` sit on this document's
    // deepest chain -- the branch that returns parsed evidence -- and `SessionRecordMeta` is
    // reached from two of the three branches, so naming it states once that they return the same
    // session facts rather than leaving a reader to compare two inline copies.
    let definitions = json!({
        "MessageReference": described(
            message_reference_output_schema(),
            "One file, URL or identifier parsed out of a message."
        ),
        "ParsedReference": parsed_reference_output_schema(),
        "SessionRecordMeta": described(
            session_record_meta_output_schema(),
            "The session facts that identify and date one indexed session."
        ),
        "MessageContentExtent": message_content_extent_output_schema()
    });
    json!({
        "type": "object",
        "$defs": definitions,
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "session": { "$ref": "#/$defs/SessionRecordMeta" },
                    "transcript": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" },
                            "total_lines": { "type": "integer", "minimum": 0 },
                            "lines_returned": { "type": "integer", "minimum": 0 },
                            "selected_edge": { "type": "string", "enum": ["head", "tail", "all"] },
                            "complete": { "type": "boolean" }
                        },
                        "required": ["text", "total_lines", "lines_returned", "selected_edge", "complete"],
                        "additionalProperties": false
                    },
                    "rendered_text": { "type": "string" }
                },
                "required": ["session", "transcript", "rendered_text"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "anchor_seq": { "type": "integer" },
                    "cwd": { "type": ["string", "null"] },
                    "repo": { "type": ["string", "null"] },
                    "title": { "type": ["string", "null"] },
                    "session_metadata": { "$ref": "#/$defs/SessionRecordMeta" },
                    "messages": { "type": "array", "items": focused_message_output_schema() }
                },
                "required": ["session_id", "anchor_seq", "cwd", "repo", "title", "session_metadata", "messages"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "session": session_record_output_schema(),
                    "user_intent": { "type": "array", "items": message_preview_output_schema() },
                    "tool_activity": { "type": "array", "items": tool_activity_output_schema() },
                    "refs": { "type": "array", "items": { "$ref": "#/$defs/ParsedReference" } },
                    "changed_files": { "type": "array", "items": changed_file_output_schema() },
                    "truncated_evidence": truncated_evidence_output_schema(),
                    "time_profile": session_time_profile_output_schema(),
                    "next_commands": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["session", "user_intent", "tool_activity", "refs", "changed_files", "truncated_evidence", "next_commands"],
                "additionalProperties": false
            }
        ]
    })
}

fn session_record_meta_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider_session_id": { "type": "string" },
            "cwd": { "type": "string" },
            "repo": { "type": "string" },
            "title": { "type": "string" },
            "updated_at": { "type": "string" },
            "last_message_at": { "type": "string" },
            "source_path": { "type": "string" },
            "message_count": { "type": "integer", "minimum": 0 },
            "parse_warning": { "type": "string" }
        },
        "additionalProperties": false
    })
}

fn session_record_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": { "type": "string" },
            "provider": provider_id_output_schema(),
            "provider_session_id": { "type": "string" },
            "title": { "type": ["string", "null"] },
            "summary": { "type": ["string", "null"] },
            "cwd": { "type": ["string", "null"] },
            "repo_root": { "type": ["string", "null"] },
            "created_at": { "type": ["string", "null"] },
            "updated_at": { "type": ["string", "null"] },
            "last_message_at": { "type": ["string", "null"] },
            "preview_text": { "type": "string" },
            "source_path": { "type": "string" },
            "message_count": { "type": ["integer", "null"], "minimum": 0 },
            "parse_version": { "type": "string" },
            "raw_metadata_json": { "type": ["string", "null"] },
            "parse_warning": { "type": ["string", "null"] },
            "discovery_source": { "type": "string" },
            "parent_session_id": { "type": ["string", "null"], "description": "The session that spawned this one when it is a subagent run, otherwise null. Providers mark subagent runs differently; this is the one field they all produce, so it answers \"every subagent of this session\" uniformly." },
            "agent_label": { "type": ["string", "null"], "description": "Human-meaningful name for the spawned agent when the provider records one, otherwise null. Display and grouping only; the link is parent_session_id." }
        },
        "required": [
            "id", "provider", "provider_session_id", "title", "summary", "cwd", "repo_root",
            "created_at", "updated_at", "last_message_at", "preview_text", "source_path",
            "message_count", "parse_version", "parse_warning",
            "discovery_source", "parent_session_id", "agent_label"
        ],
        "additionalProperties": false
    })
}

/// Schema for one `search_sessions` hit: the full session record (reused from
/// `session_record_output_schema`, the single source of truth also used by
/// get_session and `aise search --format json`) plus the search-only fields that
/// `SearchHit` flattens alongside it.
fn search_hit_output_schema() -> Value {
    let mut schema = session_record_output_schema();
    let object = schema.as_object_mut().expect("record schema is an object");
    object
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .expect("record schema has properties")
        .extend([
            (
                "score".to_string(),
                json!({ "type": "integer", "description": "Relevance score; higher scores rank first." }),
            ),
            (
                "match_source".to_string(),
                json!({ "type": "string", "description": "Which indexed field produced the match, e.g. title or content." }),
            ),
            (
                "match_snippet".to_string(),
                json!({ "type": "string", "description": "Excerpt of text around the match." }),
            ),
        ]);
    if let Some(required) = object.get_mut("required").and_then(Value::as_array_mut) {
        required.extend([
            json!("score"),
            json!("match_source"),
            json!("match_snippet"),
        ]);
    }
    schema
}

/// Schema for `search_sessions` structured output: the ranked hits plus a count.
/// Each element mirrors `aise search --format json` exactly.
fn search_sessions_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessions": { "type": "array", "description": "Matching sessions ranked by relevance, each the full session record plus score and match provenance. Element shape mirrors `aise search --format json`.", "items": search_hit_output_schema() },
            "returned": { "type": "integer", "minimum": 0, "description": "Number of sessions returned after the limit." },
            "has_more": { "type": "boolean", "description": "True when the requested limit omitted lower-ranked matches from the index state observed by this call." },
            "next_offset": { "type": ["integer", "null"], "minimum": 0, "description": "Offset for the next lower-ranked page over the same fixed index, or null when this call reached the end. The index is not snapshotted across calls; inspect pagination.consistency before assuming continuation stability." },
            "pagination": {
                "type": "object",
                "properties": {
                    "offset": { "type": "integer", "minimum": 0 },
                    "order": { "type": "string", "enum": ["score-desc,updated-at-desc,id-asc"], "description": "Total ranking order used within one fixed index state." },
                    "consistency": { "type": "string", "enum": ["per-call"], "description": "Each call is internally deterministic, but automatic indexing or other writers can change membership and ranks between calls. Numeric offsets are stable only while the index is unchanged." }
                },
                "required": ["offset", "order", "consistency"],
                "additionalProperties": false
            }
        },
        "required": ["sessions", "returned", "has_more", "next_offset", "pagination"],
        "additionalProperties": false
    })
}

/// Schema for `list_sessions` structured output: newest-first session records
/// plus a count. Each element mirrors `aise list --format json` exactly.
fn list_sessions_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "sessions": { "type": "array", "description": "Indexed sessions newest first, each a full session record. Element shape mirrors `aise list --format json`.", "items": session_record_output_schema() },
            "returned": { "type": "integer", "minimum": 0, "description": "Number of sessions returned after the limit." },
            "has_more": { "type": "boolean", "description": "True when another chronological page exists." },
            "next_offset": { "type": ["integer", "null"], "minimum": 0, "description": "Offset for the next chronological page, or null when complete." }
        },
        "required": ["sessions", "returned", "has_more", "next_offset"],
        "additionalProperties": false
    })
}

/// Schema for `get_resume_command` structured output: the resolved session and a
/// copy-pastable resume command identical to the text content.
fn get_resume_command_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string", "description": "Canonical session ID that was resolved from the requested ID or prefix." },
            "resume_command": { "type": "string", "description": "Copy-pastable POSIX-shell command that resumes the session, byte-for-byte identical to the text content. Carries the provider's resume verb and this session's id only: it does not reproduce local permission or approval flags the user normally passes, so running it verbatim may resume under different behavior than their usual invocation." },
            "cwd": { "type": ["string", "null"], "description": "Working directory the resume command changes into first, or null when none is recorded." }
        },
        "required": ["session_id", "resume_command", "cwd"],
        "additionalProperties": false
    })
}

/// One message inside a focused `get_session` read.
///
/// `refs` points at the document's named reference shape rather than inlining it. This is the
/// branch that carried `get_session`'s deepest chain: the inline copy ran messages -> refs ->
/// host -> type, four levels past what naming it costs.
fn focused_message_output_schema() -> Value {
    let mut properties = message_row_properties();
    properties.remove("session_id");
    properties.insert(
        "refs".into(),
        json!({ "type": "array", "items": { "$ref": "#/$defs/MessageReference" } }),
    );
    properties.insert(
        "content_extent".into(),
        json!({ "$ref": "#/$defs/MessageContentExtent" }),
    );
    properties.insert("is_match".into(), json!({ "type": "boolean" }));
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "seq", "role", "kind", "provider", "ts", "tool_name", "tool_call_id", "content",
            "content_extent", "is_match"
        ],
        "additionalProperties": false
    })
}

fn message_preview_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "seq": { "type": "integer", "minimum": 0 },
            "ts": { "type": ["string", "null"] },
            "chars": { "type": "integer", "minimum": 0 },
            "preview": { "type": "string" },
            "expand_command": { "type": "string" }
        },
        "required": ["seq", "ts", "chars", "preview", "expand_command"],
        "additionalProperties": false
    })
}

fn tool_activity_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "seq": { "type": "integer", "minimum": 0 },
            "ts": { "type": ["string", "null"] },
            "tool_name": { "type": ["string", "null"] },
            "kind": { "type": "string" },
            "chars": { "type": "integer", "minimum": 0 },
            "preview": { "type": "string" },
            "expand_command": { "type": "string" }
        },
        "required": ["seq", "ts", "tool_name", "kind", "chars", "preview", "expand_command"],
        "additionalProperties": false
    })
}

/// The references parsed from one message, as a named shape.
///
/// `refs` points at `#/$defs/MessageReference`, so every document that names this one must define
/// that too. Both callers do, and the pair is what keeps this shape out of the deepest chain in
/// `search_messages` and `get_session` alike.
fn parsed_reference_output_schema() -> Value {
    json!({
        "description": "The references parsed from one message, with the command that expands it.",
        "type": "object",
        "properties": {
            "seq": { "type": "integer", "minimum": 0 },
            "role": { "type": "string" },
            "tool_name": { "type": ["string", "null"] },
            "ref_summary": { "type": "string" },
            "refs": { "type": "array", "items": { "$ref": "#/$defs/MessageReference" } },
            "preview": { "type": "string" },
            "expand_command": { "type": "string" }
        },
        "required": ["seq", "role", "tool_name", "ref_summary", "refs", "preview", "expand_command"],
        "additionalProperties": false
    })
}

fn changed_file_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "file_path": { "type": "string" },
            "provider": { "type": "string" },
            "edits": { "type": "integer", "minimum": 0 },
            "follow_up_command": { "type": "string" }
        },
        "required": ["file_path", "provider", "edits", "follow_up_command"],
        "additionalProperties": false
    })
}

fn truncated_evidence_output_schema() -> Value {
    json!({
        "type": "array",
        "description": "Evidence categories with additional indexed entries omitted by summary_items. Empty means the compact summary contains every matching evidence entry; use next_commands or item expand_command values when categories are listed.",
        "items": {
            "type": "string",
            "enum": ["user_intent", "tool_activity", "reference_messages", "references", "changed_files"]
        },
        "uniqueItems": true
    })
}

fn session_time_profile_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "messages": { "type": "integer", "minimum": 0 },
            "timestamped_messages": { "type": "integer", "minimum": 0 },
            "undated_messages": { "type": "integer", "minimum": 0 },
            "first_timestamp": { "type": ["string", "null"] },
            "last_timestamp": { "type": ["string", "null"] },
            "observed_span_seconds": { "type": ["integer", "null"], "minimum": 0 },
            "max_message_gap_seconds": { "type": ["integer", "null"], "minimum": 0 },
            "tool_calls": { "type": "integer", "minimum": 0 },
            "tool_results": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "messages", "timestamped_messages", "undated_messages", "first_timestamp",
            "last_timestamp", "observed_span_seconds", "max_message_gap_seconds", "tool_calls",
            "tool_results"
        ],
        "additionalProperties": false
    })
}

fn provider_id_output_schema() -> Value {
    let providers: Vec<_> = crate::source::PROVIDERS
        .into_iter()
        .map(|provider| provider.as_str())
        .collect();
    json!({ "type": "string", "enum": providers })
}

fn message_reference_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string" },
            "value": { "type": "string" },
            "normalized_value": { "type": ["string", "null"] },
            "host": { "type": ["string", "null"] },
            "source_field": { "type": ["string", "null"] },
            "source_tool": { "type": ["string", "null"] },
            "confidence": { "type": "string" },
            "span_start": { "type": "integer", "minimum": 0 },
            "span_end": { "type": "integer", "minimum": 0 }
        },
        "required": [
            "kind", "value", "normalized_value", "host", "source_field", "source_tool",
            "confidence", "span_start", "span_end"
        ],
        "additionalProperties": false
    })
}

fn message_row_properties() -> serde_json::Map<String, Value> {
    let mut properties = serde_json::Map::new();
    properties.insert("session_id".into(), json!({ "type": "string" }));
    properties.insert("seq".into(), json!({ "type": "integer", "minimum": 0 }));
    properties.insert(
        "role".into(),
        json!({ "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"] }),
    );
    properties.insert(
        "kind".into(),
        json!({ "type": "string", "enum": message_kind_values() }),
    );
    properties.insert("provider".into(), provider_id_output_schema());
    properties.insert("ts".into(), json!({ "type": ["string", "null"] }));
    properties.insert("tool_name".into(), json!({ "type": ["string", "null"] }));
    properties.insert("tool_call_id".into(), json!({ "type": ["string", "null"] }));
    properties.insert("content".into(), json!({ "type": "string" }));
    properties.insert(
        "content_extent".into(),
        message_content_extent_output_schema(),
    );
    properties.insert("ref_summary".into(), json!({ "type": "string" }));
    properties.insert(
        "refs".into(),
        json!({ "type": "array", "items": message_reference_output_schema() }),
    );
    properties
}

fn message_content_extent_output_schema() -> Value {
    json!({
        "type": "object",
        "description": "Absolute Unicode-scalar range occupied by returned content plus the direction of any additional message-field text. The range length equals the returned content character count. field_total_chars is null only when computing it would require another full-field scan solely for metadata.",
        "properties": {
            "field_start_char": { "type": "integer", "minimum": 0, "description": "Inclusive Unicode-scalar offset of returned content in the complete message content field." },
            "field_end_char_exclusive": { "type": "integer", "minimum": 0, "description": "Exclusive Unicode-scalar offset of returned content in the complete message content field." },
            "additional_field_text": { "type": "string", "enum": ["none", "before", "after", "before_and_after"], "description": "Where additional message content exists outside the returned range. none means the range contains the complete field." },
            "field_total_chars": { "type": ["integer", "null"], "minimum": 0, "description": "Complete message content field size in coordinate_unit, or null when intentionally not rescanned solely for metadata." },
            "coordinate_unit": { "type": "string", "enum": ["unicode_scalar"], "description": "Unit of every offset and count on this extent." }
        },
        "required": [
            "field_start_char", "field_end_char_exclusive", "additional_field_text",
            "field_total_chars", "coordinate_unit"
        ],
        "additionalProperties": false
    })
}

/// Schema for one `normalized_session_metadata` entry, which serializes [`SessionMeta`] directly.
///
/// Every field of that struct is an `Option` that serde always emits, so each is `required` and
/// each admits null. This is the deduplicated per-session view carried beside message hits; the
/// full record lives in [`session_record_output_schema`], and `session_record_meta_output_schema`
/// describes the separately built object that renames `repo_root` to `repo`.
fn session_meta_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider_session_id": { "type": ["string", "null"] },
            "cwd": { "type": ["string", "null"] },
            "repo_root": { "type": ["string", "null"] },
            "title": { "type": ["string", "null"] },
            "updated_at": { "type": ["string", "null"] },
            "last_message_at": { "type": ["string", "null"] },
            "message_count": { "type": ["integer", "null"], "minimum": 0 },
            "parse_warning": { "type": ["string", "null"] },
            "parent_session_id": { "type": ["string", "null"], "description": "The session that spawned this one when it is a subagent run, otherwise null. Distinguishes a hit in a root conversation from one in a spawned or compaction fragment without a follow-up call." },
            "agent_label": { "type": ["string", "null"], "description": "Human-meaningful name for the spawned agent when the provider records one, otherwise null. Display and grouping only; the link is parent_session_id." }
        },
        "required": [
            "provider_session_id", "cwd", "repo_root", "title", "updated_at", "last_message_at",
            "message_count", "parse_warning", "parent_session_id", "agent_label"
        ],
        "additionalProperties": false
    })
}

fn search_explanation_output_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "properties": {
            "corpus": { "type": "integer", "minimum": 0 },
            "prefilter": { "type": ["string", "null"] },
            "candidates": { "type": ["integer", "null"], "minimum": 0 },
            "prefilter_skipped": { "type": ["string", "null"] },
            "summary": { "type": "string" }
        },
        "required": ["corpus", "prefilter", "candidates", "prefilter_skipped", "summary"],
        "additionalProperties": false
    })
}

fn value_origin_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "enum": ["explicit", "purpose", "surface-config", "operation-config", "typed-default", "derived"] },
            "purpose": { "type": "string" },
            "purpose_version": { "type": "integer", "minimum": 1 },
            "surface": { "type": "string", "enum": ["rust", "cli", "mcp", "python"] }
        },
        "required": ["source"],
        "additionalProperties": false
    })
}

fn search_origins_output_schema() -> Value {
    let origin = value_origin_output_schema();
    json!({
        "type": ["object", "null"],
        "description": "Resolved source of each configurable message-search parameter when receipt_level is full; null for none or summary.",
        "properties": {
            "result_extent": origin.clone(),
            "context_messages_before": origin.clone(),
            "context_messages_after": origin.clone(),
            "includes": origin.clone(),
            "detail": origin.clone(),
            "lines_per_message": origin.clone(),
            "field_view": origin.clone(),
            "match_view": origin.clone(),
            "receipt_level": origin.clone(),
            "result_order": origin
        },
        "required": [
            "result_extent", "context_messages_before", "context_messages_after", "includes",
            "detail", "lines_per_message", "field_view", "match_view", "receipt_level",
            "result_order"
        ],
        "additionalProperties": false
    })
}

fn skill_selector_input_schema() -> Value {
    json!({
        "type": "object",
        "description": "Exactly one skill selector: use name for the embedded or configured catalog, or path for a package under configured [skills].search_paths.",
        "oneOf": [
            {
                "type": "object",
                "properties": { "name": { "type": "string", "minLength": 1 } },
                "required": ["name"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": { "path": { "type": "string", "minLength": 1 } },
                "required": ["path"],
                "additionalProperties": false
            }
        ],
        "properties": {
            "name": { "type": "string", "minLength": 1 },
            "path": { "type": "string", "minLength": 1 }
        },
        "additionalProperties": false
    })
}

/// Give a named schema its one-sentence description.
///
/// A `$defs` entry without one trades a deep document for an opaque one: the reader who follows
/// the reference arrives at a shape whose name is the only thing telling them what it is.
fn described(mut schema: Value, description: &str) -> Value {
    if let Some(object) = schema.as_object_mut() {
        object.insert("description".to_owned(), json!(description));
    }
    schema
}

fn message_field_view_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": { "type": "string" },
            "field_start_char": { "type": "integer", "minimum": 0 },
            "field_end_char_exclusive": { "type": "integer", "minimum": 0 },
            "markers": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "view_start_char": { "type": "integer", "minimum": 0 },
                        "view_end_char_exclusive": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["view_start_char", "view_end_char_exclusive"],
                    "additionalProperties": false
                }
            },
            "additional_field_text": {
                "type": "string",
                "enum": ["none", "before", "after", "before_and_after"],
                "description": "Where additional selected-field text exists outside this view. none means the view contains the complete field."
            },
            // Per view, not hoisted with the unit. The two views are filled by different code
            // paths, so one can be null while the other is populated for the same result.
            "field_total_chars": {
                "type": ["integer", "null"],
                "minimum": 0,
                "description": "Complete selected-field size in the response's coordinate_unit, or null when it was intentionally not rescanned solely for metadata."
            }
        },
        "required": [
            "text", "field_start_char", "field_end_char_exclusive", "additional_field_text",
            "field_total_chars"
        ],
        "additionalProperties": false
    })
}

fn run_skill_capability_output_schema() -> Value {
    let selector = skill_selector_input_schema();
    let selected_location = json!({
        "oneOf": [
            {
                "type": "object",
                "properties": { "kind": { "type": "string", "enum": ["embedded"] } },
                "required": ["kind"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["path"] },
                    "canonical_skill_md": { "type": "string", "minLength": 1 }
                },
                "required": ["kind", "canonical_skill_md"],
                "additionalProperties": false
            }
        ]
    });
    let execution_source = json!({
        "oneOf": [
            {
                "type": "object",
                "properties": { "kind": { "type": "string", "enum": ["embedded"] } },
                "required": ["kind"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["path"] },
                    "canonical_capability_toml": { "type": "string", "minLength": 1 }
                },
                "required": ["kind", "canonical_capability_toml"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": { "kind": { "type": "string", "enum": ["inline"] } },
                "required": ["kind"],
                "additionalProperties": false
            }
        ]
    });
    let receipt = json!({
        "type": "object",
        "properties": {
            "name": { "type": "string" },
            "version": { "type": "string" },
            "sha256": { "type": "string" }
        },
        "required": ["name", "version", "sha256"],
        "additionalProperties": false
    });
    // This document was the deepest of the eight, and its depth is structural rather than
    // accidental: the payload sits five wrappers down at run -> output -> result -> report ->
    // matches before the message shapes begin, and those shapes carry their own four levels. One
    // extraction cannot fix it, so the chain is cut twice -- once at the report, once at the
    // match -- and the message field view is shared with the definition search_messages names.
    let field_view = json!({ "$ref": "#/$defs/MessageFieldView" });
    let correction_match = json!({
        "type": "object",
        "properties": {
            "message_ref": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "message_seq": { "type": "integer" }
                },
                "required": ["session_id", "message_seq"],
                "additionalProperties": false
            },
            "message_metadata": {
                "type": "object",
                "properties": {
                    "provider": { "type": "string", "enum": crate::source::PROVIDERS },
                    "timestamp": { "type": ["string", "null"], "format": "date-time" }
                },
                "required": ["provider", "timestamp"],
                "additionalProperties": false
            },
            "classification": {
                "type": "object",
                "properties": {
                    "policy_name": { "type": "string" },
                    "category": { "type": "string" },
                    "matched_text": { "type": "string" },
                    "field_start_char": { "type": "integer", "minimum": 0 },
                    "field_end_char_exclusive": { "type": "integer", "minimum": 0 },
                    "coordinate_unit": { "const": "unicode_scalar" }
                },
                "required": [
                    "policy_name", "category", "matched_text", "field_start_char",
                    "field_end_char_exclusive", "coordinate_unit"
                ],
                "additionalProperties": false
            },
            "presentation": {
                "type": "object",
                "properties": {
                    "field_view": field_view.clone(),
                    "match_view": field_view
                },
                "required": ["field_view", "match_view"],
                "additionalProperties": false
            }
        },
        "required": ["message_ref", "message_metadata", "classification", "presentation"],
        "additionalProperties": false
    });
    let definitions = json!({
        "CapabilityReceipt": described(
            receipt,
            "The identity of one loaded capability: its name, version and content digest."
        ),
        "MessageFieldView": described(
            message_field_view_output_schema(),
            "A character window over one message field, with the matched spans inside it."
        ),
        "ClassifiedMatch": described(
            correction_match,
            "One classified message, with the policy that matched it and the text that did."
        ),
        "ClassificationReport": {
            "description": "What one message-classification run produced: the policies it applied and the messages they matched.",
            "type": "object",
            "properties": {
                "policies": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/CapabilityReceipt" }
                },
                "matches": {
                    "type": "array",
                    "items": { "$ref": "#/$defs/ClassifiedMatch" }
                }
            },
            "required": ["policies", "matches"],
            "additionalProperties": false
        },
        // Two tagged unions, each a branch per variant, so their inline form costs four levels
        // wherever it appears. Naming them also states that the skill and its capability are
        // located independently: one can be embedded while the other is a path.
        "SelectedSkillLocation": described(
            selected_location,
            "Where the resolved SKILL.md came from: embedded in this build, or a path on disk."
        ),
        "CapabilityExecutionSource": described(
            execution_source,
            "Where the executed capability came from: embedded, a path on disk, or inline in the request."
        )
    });
    json!({
        "type": "object",
        "$defs": definitions,
        "properties": {
            "run": {
                "type": "object",
                "properties": {
                    "requested_selector": selector,
                    "resolved_skill": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "package_version": { "type": ["string", "null"] },
                            "selected_location": { "$ref": "#/$defs/SelectedSkillLocation" },
                            "execution_source": { "$ref": "#/$defs/CapabilityExecutionSource" }
                        },
                        "required": ["name", "package_version", "selected_location", "execution_source"],
                        "additionalProperties": false
                    },
                    "output": {
                        "type": "object",
                        "properties": {
                            "capability": { "type": "string", "enum": ["message-classification"] },
                            "result": {
                                "type": "object",
                                "properties": {
                                    "receipt": { "$ref": "#/$defs/CapabilityReceipt" },
                                    "report": { "$ref": "#/$defs/ClassificationReport" }
                                },
                                "required": ["receipt", "report"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["capability", "result"],
                        "additionalProperties": false
                    }
                },
                "required": ["requested_selector", "resolved_skill", "output"],
                "additionalProperties": false
            },
            "returned": { "type": "integer", "minimum": 0 },
            "next_offset": { "type": ["integer", "null"], "minimum": 0 },
            "pagination": {
                "type": "object",
                "properties": {
                    "limit": { "type": ["integer", "null"], "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "ordering": { "type": "string", "enum": ["timestamp desc, session id asc, sequence asc"] }
                },
                "required": ["limit", "offset", "ordering"],
                "additionalProperties": false
            }
        },
        "required": ["run", "returned", "next_offset", "pagination"],
        "additionalProperties": false
    })
}

fn search_messages_output_schema() -> Value {
    // The canonical response uses custom conditional `Serialize` projections. Deriving a schema
    // through `schemars` would require a parallel DTO and recreate the semantic drift this adapter
    // removed. Keep this protocol declaration separate from runtime construction, then validate
    // every conditional runtime branch against it in
    // `search_messages_runtime_variants_conform_to_the_closed_output_schema`.
    // Four shapes are named rather than inlined, because each is reached from more than one place
    // and two of them sit on the chain that made this document 18 levels deep. A `$ref` is a leaf
    // where it is used, so naming the neighbouring-message shape collapses the whole
    // results -> context -> messages_after -> presentation -> field_view -> markers chain into one
    // step at the point of use, and the definition it points at is shallow on its own.
    //
    // Only shapes a reader would recognise are named. A mechanical pass over every object reaches
    // similar numbers and leaves two dozen anonymous definitions behind, which trades a deep
    // document for an unreadable one.
    let field_view = json!({ "$ref": "#/$defs/MessageFieldView" });
    let message_ref = json!({ "$ref": "#/$defs/MessageRef" });
    let message_metadata = json!({ "$ref": "#/$defs/MessageMetadata" });
    let context_message = json!({ "$ref": "#/$defs/ContextMessage" });
    let definitions = json!({
        "MessageRef": {
            "description": "The coordinates get_session takes: one session and one message sequence within it.",
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "message_seq": { "type": "integer" }
            },
            "required": ["session_id", "message_seq"],
            "additionalProperties": false
        },
        "MessageMetadata": {
            "description": "Which source a message came from, who authored it, and its semantic class.",
            "type": "object",
            "properties": {
                "provider": { "type": "string", "enum": crate::source::PROVIDERS },
                "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"] },
                "kind": { "type": "string", "enum": message_kind_values() }
            },
            "required": ["provider", "role", "kind"],
            "additionalProperties": false
        },
        "MessageFieldView": described(
            message_field_view_output_schema(),
            "A character window over one message field, with the matched spans inside it."
        ),
        "MessageReference": described(
            message_reference_output_schema(),
            "One file, URL or identifier parsed out of a message."
        ),
        "ParsedReference": parsed_reference_output_schema(),
        "ContextMessage": {
            "description": "One neighbouring message returned around a hit, which never changes hit membership.",
            "type": "object",
            "properties": {
                "message_ref": { "$ref": "#/$defs/MessageRef" },
                "message_metadata": { "$ref": "#/$defs/MessageMetadata" },
                "timestamp": { "type": ["string", "null"], "format": "date-time" },
                "tool_name": { "type": ["string", "null"] },
                "tool_call_id": { "type": ["string", "null"] },
                "presentation": {
                    "type": "object",
                    "properties": {
                        "field_view": { "$ref": "#/$defs/MessageFieldView" }
                    },
                    "required": ["field_view"],
                    "additionalProperties": false
                }
            },
            "required": [
                "message_ref", "message_metadata", "timestamp", "tool_name", "tool_call_id",
                "presentation"
            ],
            "additionalProperties": false
        }
    });
    let result = json!({
        "type": "object",
        "properties": {
            "message_ref": message_ref,
            "message_metadata": message_metadata,
            "match": {
                "type": "object",
                "properties": {
                    "field": { "type": "string", "enum": ["content", "tool_name", "tool_argument"] },
                    "argument_path": { "type": "string" },
                    "fuzzy_score": { "type": "integer", "minimum": 0 },
                    "literal_occurrence": {
                        "type": "object",
                        "properties": {
                            "text": { "type": "string" },
                            "field_start_char": { "type": "integer", "minimum": 0 },
                            "field_end_char_exclusive": { "type": "integer", "minimum": 0 },
                        },
                        "required": [
                            "text", "field_start_char", "field_end_char_exclusive"
                        ],
                        "additionalProperties": false
                    }
                },
                "required": ["field"],
                "additionalProperties": false
            },
            "presentation": {
                "type": "object",
                "properties": {
                    "field_view": field_view.clone(),
                    "match_view": field_view
                },
                "required": ["field_view"],
                "additionalProperties": false
            },
            "included": {
                "type": "object",
                "properties": {
                    "parsed_references": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/ParsedReference" }
                    }
                },
                "required": ["parsed_references"],
                "additionalProperties": false
            },
            "context": {
                "type": "object",
                "properties": {
                    "messages_before": { "type": "array", "items": context_message.clone() },
                    "messages_after": { "type": "array", "items": context_message }
                },
                "required": ["messages_before", "messages_after"],
                "additionalProperties": false
            }
        },
        "required": ["message_ref", "message_metadata", "presentation"],
        "additionalProperties": false
    });
    json!({
        "type": "object",
        "$defs": definitions,
        "properties": {
            "response_schema_version": { "type": "integer", "description": "Version of this search_messages response contract, independent of the SQLite database schema version." },
            // Stated once for the whole document. Every character offset in it -- view
            // boundaries, match coordinates, field totals -- is in this unit.
            "coordinate_unit": { "type": "string", "enum": ["unicode_scalar"], "description": "Unit of every character offset and count in this response." },
            "effective_request": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "query_mode": { "type": "string", "enum": ["literal", "regex", "fuzzy"] },
                    "target": {
                        "type": "object",
                        "properties": {
                            "field": { "type": "string", "enum": ["content", "tool_name", "tool_argument"] },
                            "argument_path": { "type": ["string", "null"] }
                        },
                        "required": ["field"],
                        "additionalProperties": false
                    },
                    "provider_scope": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["all", "selected"] },
                            "providers": { "type": "array", "items": { "type": "string", "enum": crate::source::PROVIDERS } }
                        },
                        "required": ["kind"],
                        "additionalProperties": false
                    },
                    "extent": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["page", "all_results"] },
                            "limit": { "type": "integer", "minimum": 1 },
                            "offset": { "type": "integer", "minimum": 0 }
                        },
                        "required": ["kind", "offset"],
                        "additionalProperties": false
                    },
                    "match_window": { "type": "string", "enum": ["earliest", "latest"] },
                    "context": {
                        "type": "object",
                        "properties": {
                            "messages_before": { "type": "integer", "minimum": 0 },
                            "messages_after": { "type": "integer", "minimum": 0 }
                        },
                        "required": ["messages_before", "messages_after"],
                        "additionalProperties": false
                    },
                    "presentation": {
                        "type": "object",
                        "properties": {
                            "lines_per_message": { "type": "integer" },
                            "field_view": {
                                "type": "object",
                                "properties": {
                                    "kind": { "type": "string", "enum": ["no_char_limit", "max_chars"] },
                                    "max_chars": { "type": "integer", "minimum": 1 }
                                },
                                "required": ["kind"],
                                "additionalProperties": false
                            },
                            "match_view": {
                                "type": "object",
                                "properties": {
                                    "kind": { "type": "string", "enum": ["minimal_span", "max_chars"] },
                                    "max_chars": { "type": "integer", "minimum": 1 }
                                },
                                "required": ["kind"],
                                "additionalProperties": false
                            }
                        },
                        "required": ["lines_per_message", "field_view", "match_view"],
                        "additionalProperties": false
                    },
                    "include": {
                        "type": "array",
                        "items": {
                            "type": "string",
                            "enum": [
                                "normalized_session_metadata", "parsed_references",
                                "raw_provider_metadata", "runtime_diagnostics"
                            ]
                        }
                    },
                    "receipt_level": { "type": "string", "enum": ["none", "summary", "full"] }
                },
                "required": [
                    "target", "provider_scope", "extent", "context", "presentation", "include",
                    "receipt_level"
                ],
                "additionalProperties": false
            },
            "results": { "type": "array", "items": result },
            "page": {
                "type": "object",
                "properties": {
                    "returned": { "type": "integer", "minimum": 0 },
                    "limit": { "type": ["integer", "null"], "minimum": 1 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "has_more": { "type": "boolean" },
                    "next_offset": { "type": ["integer", "null"], "minimum": 0 },
                    "earlier_results": { "type": "string", "enum": ["none", "present", "unknown"] },
                    "result_set_extent": { "type": "string", "enum": ["all", "partial", "unknown"] },
                    "ordering": {
                        "type": "string",
                        "enum": ["session-sequence", "fuzzy-relevance"]
                    },
                    "consistency": { "const": "per-call" }
                },
                "required": [
                    "returned", "limit", "offset", "has_more", "next_offset", "earlier_results",
                    "result_set_extent", "ordering", "consistency"
                ],
                "additionalProperties": false
            },
            "included": {
                "type": "object",
                "properties": {
                    "normalized_session_metadata": { "type": "object", "additionalProperties": session_meta_output_schema() },
                    "raw_provider_metadata": { "type": "object", "additionalProperties": true },
                    "runtime_diagnostics": {
                        "type": "object",
                        "properties": {
                            "package_version": { "type": "string" },
                            "database_schema_version": { "type": "integer" },
                            "response_schema_version": { "type": "integer" },
                            "surface": { "const": "mcp" },
                            "config_digest": { "type": "string" }
                        },
                        "required": [
                            "package_version", "database_schema_version", "response_schema_version",
                            "surface", "config_digest"
                        ],
                        "additionalProperties": false
                    }
                },
                "additionalProperties": false
            },
            "receipt": {
                "type": "object",
                "properties": {
                    "search_explanation": search_explanation_output_schema(),
                    "parameter_origins": search_origins_output_schema(),
                    "ordered_digest": { "type": "string" }
                },
                "additionalProperties": false
            }
        },
        "required": ["response_schema_version", "coordinate_unit", "effective_request", "results", "page"],
        "additionalProperties": false
    })
}

fn get_index_status_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "db_path": { "type": "string", "description": "Resolved SQLite index path used by this server process." },
            "parser_health": parser_health_output_schema(),
            "repairable_stale_sessions": { "type": "integer", "minimum": 0, "description": "Indexed sessions whose source file is discoverable and can be reparsed." },
            "unavailable_stale_sessions": { "type": "integer", "minimum": 0, "description": "Retained indexed sessions whose original source file is unavailable; reindexing cannot recreate them." },
            "unindexed_files": { "type": "integer", "minimum": 0, "description": "Discovered files that produced no session at all, so their content is absent from every search result. This is not discovered_files minus indexed_sessions: retained sessions make indexed exceed discovered. Non-zero means the index is incomplete and repair_commands names the repair." },
            "repair_commands": { "type": "array", "description": "Commands applicable to the reported stale schema or discoverable source files; empty means no repair is required.", "items": { "type": "string" } },
            "readiness": index_readiness_output_schema(),
            "providers": { "type": "array", "description": "Discovery, parser, index, and resume status for every supported provider.", "items": provider_health_output_schema() }
        },
        "required": ["db_path", "parser_health", "repairable_stale_sessions", "unavailable_stale_sessions", "unindexed_files", "repair_commands", "readiness", "providers"],
        "additionalProperties": false
    })
}

fn index_readiness_output_schema() -> Value {
    json!({
        "type": "object",
        "description": "Orthogonal snapshot usability and automatic refresh state. A usable snapshot remains searchable while refresh is indexing, postponed, or failed.",
        "properties": {
            "snapshot": {
                "type": "object",
                "properties": {
                    "availability": { "type": "string", "enum": ["unavailable", "usable"], "description": "unavailable means no complete snapshot exists and searches must not return false-empty results; usable means a compatible snapshot can be searched even while refresh runs." },
                    "last_successful_refresh_at": { "type": ["string", "null"], "format": "date-time" }
                },
                "required": ["availability", "last_successful_refresh_at"],
                "additionalProperties": false
            },
            "refresh": {
                "type": "object",
                "properties": {
                    "state": { "type": "string", "enum": ["not_started", "indexing", "fresh", "postponed", "failed_with_recovery"] },
                    "started_by": { "type": ["string", "null"], "enum": ["integration_install", "command_line", "mcp", null] },
                    "started_at": { "type": ["string", "null"], "format": "date-time" },
                    "finished_at": { "type": ["string", "null"], "format": "date-time" },
                    "files_discovered": { "type": ["integer", "null"], "minimum": 0 },
                    "files_processed": { "type": ["integer", "null"], "minimum": 0 },
                    "sessions_updated": { "type": ["integer", "null"], "minimum": 0 },
                    "retry_after_ms": { "type": ["integer", "null"], "minimum": 0 },
                    "message": { "type": ["string", "null"] },
                    "next_command": { "type": ["string", "null"] }
                },
                "required": ["state", "started_by", "started_at", "finished_at", "files_discovered", "files_processed", "sessions_updated", "retry_after_ms", "message", "next_command"],
                "additionalProperties": false
            }
        },
        "required": ["snapshot", "refresh"],
        "additionalProperties": false
    })
}

fn provider_parser_health_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider": provider_id_output_schema(),
            "expected_parse_version": { "type": "string" },
            "indexed_sessions": { "type": "integer", "minimum": 0 },
            "current_sessions": { "type": "integer", "minimum": 0 },
            "stale_sessions": { "type": "integer", "minimum": 0 }
        },
        "required": ["provider", "expected_parse_version", "indexed_sessions", "current_sessions", "stale_sessions"],
        "additionalProperties": false
    })
}

fn parser_health_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema_version": { "type": "integer", "minimum": 0 },
            "expected_schema_version": { "type": "integer", "minimum": 0 },
            "schema_current": { "type": "boolean" },
            "indexed_sessions": { "type": "integer", "minimum": 0 },
            "current_sessions": { "type": "integer", "minimum": 0 },
            "stale_sessions": { "type": "integer", "minimum": 0, "description": "Indexed sessions whose stored parse version is older than the current one. This is the total; see repairable_stale_sessions and unavailable_stale_sessions for the split. A stale session is still fully searchable, and the unavailable portion has no repair because its transcript is gone from disk, so a non-zero value here does not by itself mean action is required." },
            "parse_warnings": { "type": "integer", "minimum": 0 },
            "providers": { "type": "array", "items": provider_parser_health_output_schema() }
        },
        "required": [
            "schema_version", "expected_schema_version", "schema_current", "indexed_sessions",
            "current_sessions", "stale_sessions", "parse_warnings", "providers"
        ],
        "additionalProperties": false
    })
}

fn provider_health_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider": provider_id_output_schema(),
            "enabled": { "type": "boolean" },
            "cli_available": { "type": "boolean" },
            "roots": { "type": "array", "items": { "type": "string" } },
            "discovered_files": { "type": "integer", "minimum": 0 },
            "indexed_sessions": { "type": "integer", "minimum": 0 },
            "unindexed_files": { "type": "integer", "minimum": 0, "description": "Discovered files for this provider that produced no session. discovered_files and indexed_sessions come from different subsystems and are not two ends of one subtraction; this is their reconciliation." },
            "expected_parse_version": { "type": "string" },
            "current_sessions": { "type": "integer", "minimum": 0 },
            "stale_sessions": { "type": "integer", "minimum": 0 },
            "repairable_stale_sessions": { "type": "integer", "minimum": 0 },
            "unavailable_stale_sessions": { "type": "integer", "minimum": 0 },
            "resume_command": { "type": ["string", "null"], "description": "Command that resumes this provider's newest available session, or null when the provider cannot currently be resumed." }
        },
        "required": [
            "provider", "enabled", "cli_available", "roots", "discovered_files",
            "indexed_sessions", "expected_parse_version", "current_sessions", "stale_sessions",
            "repairable_stale_sessions", "unavailable_stale_sessions", "unindexed_files", "resume_command"
        ],
        "additionalProperties": false
    })
}

fn query_session_index_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "columns": { "type": "array", "description": "Ordered output-column names from the schema inspection or read-only SQL statement.", "items": { "type": "string" } },
            "rows": { "type": "array", "description": "Returned rows. Each object's keys are the names in columns; arbitrary read-only SQL makes those keys request-defined rather than statically enumerable.", "items": { "type": "object", "additionalProperties": true } },
            "next_offset": { "type": ["integer", "null"], "minimum": 0, "description": "Offset for the next non-overlapping row page, or null when no matching rows remain." },
            "truncated_cell_char_limit": { "type": ["integer", "null"], "minimum": 1, "description": "The max_cell_chars value that shortened at least one returned string cell, or null when every returned cell is complete. Retry with a larger value or 0 for complete cells." }
        },
        "required": ["columns", "rows", "next_offset", "truncated_cell_char_limit"],
        "additionalProperties": false
    })
}

/// MCP spellings for each canonical message-search concept.
///
/// A concept may require several idiomatic MCP properties. Keeping that split explicit prevents
/// schema generation from pretending that `context`, sequence bounds, or paging are single wire
/// fields. The table is fixed-size, so projection is `O(P + F)` time and `O(F)` bounded catalogue
/// memory for canonical parameters `P` and MCP fields `F`, independent of index or result size.
fn message_search_mcp_fields(parameter: MessageSearchParameter) -> &'static [&'static str] {
    match parameter {
        MessageSearchParameter::Query => &["query"],
        MessageSearchParameter::QueryMode => &["query_mode"],
        MessageSearchParameter::Field => &["field"],
        MessageSearchParameter::ArgumentPath => &["argument_path"],
        MessageSearchParameter::Role => &["role"],
        MessageSearchParameter::Kinds => &["kind", "kinds"],
        MessageSearchParameter::Providers => &["providers"],
        MessageSearchParameter::SessionId => &["session_id"],
        MessageSearchParameter::WorkspacePathPrefix => &["workspace_path_prefix"],
        MessageSearchParameter::TranscriptPathPrefix => &["transcript_path_prefix"],
        MessageSearchParameter::ExcludeWorkspacePathPrefixes => {
            &["exclude_workspace_path_prefixes"]
        }
        MessageSearchParameter::ExcludeTranscriptPathPrefixes => {
            &["exclude_transcript_path_prefixes"]
        }
        MessageSearchParameter::ExcludeSessionIds => &["exclude_session_ids"],
        MessageSearchParameter::Since => &["since", "when"],
        MessageSearchParameter::Until => &["until", "when"],
        MessageSearchParameter::Sequence => &["seq_from", "seq_to"],
        MessageSearchParameter::ToolNameContains => &["tool_name_contains"],
        MessageSearchParameter::IncludeCompaction => &["include_compaction"],
        MessageSearchParameter::MatchWindow => &["match_window"],
        MessageSearchParameter::Context => &["context", "context_before", "context_after"],
        MessageSearchParameter::ResultExtent => &["limit", "all_results", "offset"],
        MessageSearchParameter::Detail => &["detail"],
        MessageSearchParameter::LinesPerMessage => &["lines_per_message"],
        MessageSearchParameter::FieldView => &["field_view"],
        MessageSearchParameter::MatchView => &["match_view"],
        MessageSearchParameter::Purpose => &["purpose", "purpose_version"],
        MessageSearchParameter::ReceiptLevel => &["receipt_level"],
        MessageSearchParameter::Include => &["include"],
    }
}

fn set_schema_default(properties: &mut serde_json::Map<String, Value>, field: &str, value: Value) {
    properties
        .get_mut(field)
        .and_then(Value::as_object_mut)
        .expect("built-in message-search field has an object schema")
        .insert("default".to_owned(), value);
}

fn set_schema_description(
    properties: &mut serde_json::Map<String, Value>,
    field: &str,
    description: impl Into<String>,
) {
    properties
        .get_mut(field)
        .and_then(Value::as_object_mut)
        .expect("built-in message-search field has an object schema")
        .insert("description".to_owned(), Value::String(description.into()));
}

/// Attach planner-owned defaults and canonical semantic identities to the MCP wire schema.
///
/// This deliberately enriches the existing conservative JSON Schema instead of replacing rmcp's
/// custom catalogue/lifecycle or inventing a second request model. JSON-Schema extension keys are
/// annotations: clients that retain them can introspect the canonical concepts and rules, while
/// clients that ignore them continue to validate the same ordinary properties.
/// What one MCP wire field contributes that its canonical parameter does not say by itself.
///
/// Seven of the 28 canonical parameters reach the wire under more than one name, and an alias
/// needs one clause the shared identity cannot supply: `kind` is the one-value spelling of
/// `kinds`, `seq_from` and `seq_to` are the two ends of one range, `when` sets both time bounds
/// at once. Keying on this rather than on the field's spelling is what keeps the two meanings of
/// `since` -- session activity elsewhere, message timestamp here -- from collapsing into one
/// description, which is the failure `WP-P-share-identical-cross-tool-concepts` guards against
/// across tools and this enum guards against within one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpFieldRole {
    /// The field is the whole parameter; render from the registry entry alone.
    Canonical,
    /// A one-value spelling of a set-valued parameter, named here so the conflict can be stated.
    OneValueAlias {
        plural: &'static str,
    },
    ContextBefore,
    ContextAfter,
    AllResults,
    Offset,
    SequenceFrom,
    SequenceTo,
    TimePeriod,
    PurposeVersion,
}

/// Every advertised `search_messages` field, the canonical parameter it resolves to, and its role.
///
/// This is the inverse of [`message_search_mcp_fields`], which stays a match over the parameter
/// enum so the compiler still proves every canonical parameter names its wire spellings.
/// `advertised_and_projected_field_sets_agree` asserts the two tables describe the same set, so
/// neither can gain a field the other does not know about.
const MESSAGE_SEARCH_MCP_FIELDS: &[(&str, MessageSearchParameter, McpFieldRole)] = &[
    (
        "query",
        MessageSearchParameter::Query,
        McpFieldRole::Canonical,
    ),
    (
        "query_mode",
        MessageSearchParameter::QueryMode,
        McpFieldRole::Canonical,
    ),
    (
        "field",
        MessageSearchParameter::Field,
        McpFieldRole::Canonical,
    ),
    (
        "argument_path",
        MessageSearchParameter::ArgumentPath,
        McpFieldRole::Canonical,
    ),
    (
        "role",
        MessageSearchParameter::Role,
        McpFieldRole::Canonical,
    ),
    (
        "kinds",
        MessageSearchParameter::Kinds,
        McpFieldRole::Canonical,
    ),
    (
        "kind",
        MessageSearchParameter::Kinds,
        McpFieldRole::OneValueAlias { plural: "kinds" },
    ),
    (
        "include_compaction",
        MessageSearchParameter::IncludeCompaction,
        McpFieldRole::Canonical,
    ),
    (
        "providers",
        MessageSearchParameter::Providers,
        McpFieldRole::Canonical,
    ),
    (
        "session_id",
        MessageSearchParameter::SessionId,
        McpFieldRole::Canonical,
    ),
    (
        "workspace_path_prefix",
        MessageSearchParameter::WorkspacePathPrefix,
        McpFieldRole::Canonical,
    ),
    (
        "transcript_path_prefix",
        MessageSearchParameter::TranscriptPathPrefix,
        McpFieldRole::Canonical,
    ),
    (
        "exclude_workspace_path_prefixes",
        MessageSearchParameter::ExcludeWorkspacePathPrefixes,
        McpFieldRole::Canonical,
    ),
    (
        "exclude_transcript_path_prefixes",
        MessageSearchParameter::ExcludeTranscriptPathPrefixes,
        McpFieldRole::Canonical,
    ),
    (
        "exclude_session_ids",
        MessageSearchParameter::ExcludeSessionIds,
        McpFieldRole::Canonical,
    ),
    (
        "tool_name_contains",
        MessageSearchParameter::ToolNameContains,
        McpFieldRole::Canonical,
    ),
    (
        "since",
        MessageSearchParameter::Since,
        McpFieldRole::Canonical,
    ),
    (
        "until",
        MessageSearchParameter::Until,
        McpFieldRole::Canonical,
    ),
    (
        "when",
        MessageSearchParameter::Since,
        McpFieldRole::TimePeriod,
    ),
    (
        "seq_from",
        MessageSearchParameter::Sequence,
        McpFieldRole::SequenceFrom,
    ),
    (
        "seq_to",
        MessageSearchParameter::Sequence,
        McpFieldRole::SequenceTo,
    ),
    (
        "match_window",
        MessageSearchParameter::MatchWindow,
        McpFieldRole::Canonical,
    ),
    (
        "limit",
        MessageSearchParameter::ResultExtent,
        McpFieldRole::Canonical,
    ),
    (
        "offset",
        MessageSearchParameter::ResultExtent,
        McpFieldRole::Offset,
    ),
    (
        "all_results",
        MessageSearchParameter::ResultExtent,
        McpFieldRole::AllResults,
    ),
    (
        "context",
        MessageSearchParameter::Context,
        McpFieldRole::Canonical,
    ),
    (
        "context_before",
        MessageSearchParameter::Context,
        McpFieldRole::ContextBefore,
    ),
    (
        "context_after",
        MessageSearchParameter::Context,
        McpFieldRole::ContextAfter,
    ),
    (
        "lines_per_message",
        MessageSearchParameter::LinesPerMessage,
        McpFieldRole::Canonical,
    ),
    (
        "field_view",
        MessageSearchParameter::FieldView,
        McpFieldRole::Canonical,
    ),
    (
        "match_view",
        MessageSearchParameter::MatchView,
        McpFieldRole::Canonical,
    ),
    (
        "detail",
        MessageSearchParameter::Detail,
        McpFieldRole::Canonical,
    ),
    (
        "include",
        MessageSearchParameter::Include,
        McpFieldRole::Canonical,
    ),
    (
        "receipt_level",
        MessageSearchParameter::ReceiptLevel,
        McpFieldRole::Canonical,
    ),
    (
        "purpose",
        MessageSearchParameter::Purpose,
        McpFieldRole::Canonical,
    ),
    (
        "purpose_version",
        MessageSearchParameter::Purpose,
        McpFieldRole::PurposeVersion,
    ),
];

/// Render a declared default the way a caller would type it, without JSON quoting noise.
///
/// `"literal"` reads as `literal` and `["normalized_session_metadata"]` keeps its brackets,
/// because one is a token the caller writes bare in prose and the other is a list whose shape
/// matters.
fn render_default_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "none".to_owned(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// Render one field's description from the registry rather than transcribing it.
///
/// Two things are deliberately absent, and both are what makes the result fit the budget that
/// binds. Accepted values are not spelled out: they travel as `enum`, which is one of the
/// fourteen keys Codex models and one VS Code preserves, so restating them buys nothing and
/// costs a copy that can drift. Shared vocabulary is not repeated per field: it is stated once
/// in `tool.description`, the channel every client forwards whole. What is left on each field is
/// the one fact that distinguishes it -- what the value selects, and what omitting it does.
fn render_message_search_field_description(
    field: &str,
    role: McpFieldRole,
    parameter: &crate::message_search::MessageSearchParameterSpec,
    configured: &crate::message_search::ResolvedMessageSearchRequest,
    declared_default: Option<&Value>,
) -> String {
    use crate::message_search::MessageSearchOmission;

    let context = configured.context();
    match role {
        McpFieldRole::OneValueAlias { plural } => {
            format!("One value; conflicts with {plural}.")
        }
        McpFieldRole::ContextBefore => format!(
            "Preceding neighbours per hit; omit for {}.",
            context.messages_before()
        ),
        McpFieldRole::ContextAfter => format!(
            "Following neighbours per hit; omit for {}.",
            context.messages_after()
        ),
        McpFieldRole::AllResults => {
            "Every match, not a page; conflicts with limit and fuzzy.".to_owned()
        }
        McpFieldRole::Offset => "Zero-based page offset; pass page.next_offset.".to_owned(),
        McpFieldRole::SequenceFrom => "Lower sequence bound; requires session_id.".to_owned(),
        McpFieldRole::SequenceTo => "Upper sequence bound; requires session_id.".to_owned(),
        McpFieldRole::TimePeriod => {
            "One period, both bounds; conflicts with since and until.".to_owned()
        }
        McpFieldRole::PurposeVersion => "Purpose bundle version; required with purpose.".to_owned(),
        McpFieldRole::Canonical => {
            // A handful of fields need a number the registry does not carry, because the value is
            // this surface's planner-resolved policy rather than the parameter's semantics. Read
            // it from the resolved request rather than from raw configuration: a purpose bundle
            // or an operator override can move it, and a description that names the raw value
            // would then state a default the caller does not get.
            let omission = match (field, parameter.omission()) {
                ("limit", _) => match configured.extent() {
                    ResolvedRequestExtent::Page { limit, .. } => {
                        format!("omit for the configured page of {limit}.")
                    }
                    ResolvedRequestExtent::AllResults { .. } => {
                        "omit to return every match.".to_owned()
                    }
                },
                ("context", _) => format!("omit for {}.", context.messages_before()),
                // Three ordered levels whose contents differ. `enum` carries the tokens; what it
                // cannot carry is which level buys which diagnostic, and a caller choosing
                // between them is choosing between costs.
                ("receipt_level", _) => {
                    "summary explains the plan, full adds value origins; omit for none.".to_owned()
                }
                // A signed count has to state all four cases. `selects` describes what the
                // parameter selects, not what the sign means, and "first lines, or last lines"
                // leaves a caller to guess which sign selects which end.
                ("lines_per_message", _) => format!(
                    "positive keeps first N, negative last N, 0 none; omit for {}.",
                    configured.presentation().lines_per_message()
                ),
                ("field_view", _) => match configured.presentation().field_view() {
                    FieldViewBudget::MaxChars { max_chars } => {
                        format!("omit for max_chars {max_chars}.")
                    }
                    FieldViewBudget::NoCharLimit => "omit for no character cap.".to_owned(),
                },
                ("match_view", _) => match configured.presentation().match_view() {
                    MatchViewBudget::MaxChars { max_chars } => {
                        format!("omit for max_chars {max_chars}.")
                    }
                    MatchViewBudget::MinimalSpan => "omit for the smallest match span.".to_owned(),
                },
                (_, MessageSearchOmission::AllEligible) => "omit for all.".to_owned(),
                (_, MessageSearchOmission::NoAdditionalFilter) => "omit for none.".to_owned(),
                (_, MessageSearchOmission::QuerylessSearch) => {
                    "omit for a queryless search.".to_owned()
                }
                // A defaulted parameter must name its default value in the text, not just in the
                // keyword. `default` is one of the six keywords both Codex and VS Code delete
                // before a model sees the schema, and JSON Schema 2020-12 section 9.2 makes it an
                // annotation with no validation force even where it survives -- so "omit for the
                // default" tells the caller a default exists and never says what it is. Read it
                // from the schema's own `default`, which this projection has already resolved, so
                // the prose and the keyword cannot disagree about the value.
                (_, MessageSearchOmission::TypedDefault | MessageSearchOmission::SurfacePolicy) => {
                    match declared_default {
                        Some(value) => format!("omit for {}.", render_default_value(value)),
                        None => "omit for the configured default.".to_owned(),
                    }
                }
            };
            let selects = parameter.selects().trim_end_matches('.');
            format!("{selects}; {omission}")
        }
    }
}

/// Compose `tool.description` from the registry: the lead, the vocabulary `enum` cannot carry,
/// and the nine conflict rules verbatim.
///
/// The rules are `rule_descriptors()[i].message()` with a bullet, never a paraphrase. The moment
/// the published text is authored separately from the validator's it drifts, and the published
/// rules stop describing behaviour;
/// `search_messages_description_publishes_every_conflict_rule_verbatim` asserts substring
/// containment so generation makes that drift unrepresentable rather than merely caught.
fn message_search_tool_description(config: &Config) -> String {
    let specification = MessageService::message_search_spec_for_config(config, SearchSurface::Mcp)
        .expect("validated MCP configuration resolves a queryless message-search request");
    let registry = specification.registry();
    let mut text = String::from(
        "Find exact message evidence across local AI session history. structuredContent is \
         authoritative: effective_request states the resolved interpretation and budgets, results \
         carry message_ref plus field and match views with exact literal coordinates, and \
         page.next_offset is the next offset argument when more results exist. Expand one hit \
         with get_session(session_id, message_seq).\n",
    );
    // Only what `enum` cannot say. The six message classes and eight provider slugs already
    // travel as accepted values on their own parameters, and restating them here would be a
    // second copy of a list the model already has, free to drift from it.
    text.push_str(
        "harness_notice is what the harness told the agent rather than what a person wrote, and \
         it is the one message class omitted when kinds is omitted. query_mode=regex is Rust \
         regex syntax and query_mode=fuzzy is a bounded fuzzy match. Times accept an RFC 3339 \
         instant or a relative form; see `aise dates`. lines_per_message applies its line window \
         first, then field_view and match_view apply their character budgets, counted in Unicode \
         scalars; all three change presentation only and never change matching, ordering, result \
         count, context membership, or includes.\n",
    );
    text.push_str("Combinations that are rejected:\n");
    for descriptor in registry.rule_descriptors() {
        text.push_str("  ");
        text.push_str(descriptor.message());
        text.push('\n');
    }
    text
}

/// One meaning shared by a parameter name that appears on more than one tool.
///
/// Keyed on the concept, never on the spelling, and the two `since` entries are why. `since`
/// bounds when a *session* was last active on `list_sessions` and `search_sessions`, and when an
/// individual *message* was written on `search_messages` and `run_skill_capability`. A table
/// keyed on the string would collapse those into one description that is wrong on two tools --
/// a worse defect than the divergence it set out to remove.
///
/// The enum is closed, so a field mapped to no concept keeps whatever its own builder writes and
/// a concept with no rendering fails to compile. That is
/// `REQ044-automate-safe-problem-solving`'s "closed enums, generated descriptions" applied to
/// the seven tools `MessageSearchParameterRegistry` does not cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolParameterConcept {
    /// `since`/`until` over when a session was last active.
    SessionActivityWindow,
    /// `since`/`until` over one message's own timestamp.
    MessageTimestampWindow,
    /// One period setting both bounds, over session activity.
    SessionActivityPeriod,
    /// One period setting both bounds, over message timestamps.
    MessageTimestampPeriod,
    ReturnEveryMatch,
    WorkspacePrefixFilter,
    SessionSelector,
    OptionalPayloadGroups,
    PreviewCharBudget,
    FieldCharBudget,
    MatchCharBudget,
    LineWindow,
    PresentationPreset,
    /// The categories a directly supplied capability definition declares, which had no
    /// description at all: the one genuine undocumented-parameter defect in this catalogue.
    ExcludedSessionIds,
    SkillCategorySelector,
}

/// The one description each concept carries, on every tool that uses it.
///
/// Deliberately terse for the same reason the registry's prose is: accepted values travel as
/// `enum`, which every measured client preserves, and the shared vocabulary a tool needs sits in
/// its own `tool.description`.
pub(crate) fn concept_description(concept: ToolParameterConcept) -> &'static str {
    use ToolParameterConcept as C;
    match concept {
        C::SessionActivityWindow => {
            "Bound on when the session was last active, not on any one message; \
             RFC 3339 or a relative form, see `aise dates`. Omit for no bound."
        }
        C::MessageTimestampWindow => {
            "Bound on the message's own timestamp, not on session activity; \
             RFC 3339 or a relative form, see `aise dates`. Omit for no bound."
        }
        C::SessionActivityPeriod => {
            "One period bounding session activity at both ends; conflicts with since and until."
        }
        C::MessageTimestampPeriod => {
            "One period bounding message timestamps at both ends; conflicts with since and until."
        }
        C::ReturnEveryMatch => "true returns every match instead of a page; conflicts with limit.",
        C::WorkspacePrefixFilter => {
            "Match the session's cwd or repo root by prefix. Omit for every root."
        }
        C::SessionSelector => "One session, by ID or unique prefix.",
        C::OptionalPayloadGroups => "Optional payload groups; a supplied set replaces the default.",
        C::PreviewCharBudget => "Characters per preview. Omit for the configured budget.",
        C::FieldCharBudget => "Character budget for the selected field.",
        C::MatchCharBudget => "Character budget around the match.",
        C::LineWindow => {
            "Line window: positive keeps first N, negative last N, 0 none; conflicts with detail."
        }
        C::PresentationPreset => {
            "Presentation preset; conflicts with lines_per_message, field_view and match_view."
        }
        C::ExcludedSessionIds => "Session IDs to exclude. Omit to exclude none.",
        C::SkillCategorySelector => {
            "Typed classification categories for this call, replacing the package's own."
        }
    }
}

/// Which advertised field on which tool carries which concept.
///
/// A field absent from this table keeps whatever its own builder writes, so adoption is
/// incremental. `search_messages` is deliberately absent throughout: its registry owns every
/// description it has, and letting this table write over generated text would put two owners on
/// one string again, which is the failure the generation work exists to remove.
pub(crate) const TOOL_FIELD_CONCEPT: &[(&str, &str, ToolParameterConcept)] = &[
    (
        "list_sessions",
        "since",
        ToolParameterConcept::SessionActivityWindow,
    ),
    (
        "list_sessions",
        "until",
        ToolParameterConcept::SessionActivityWindow,
    ),
    (
        "list_sessions",
        "when",
        ToolParameterConcept::SessionActivityPeriod,
    ),
    (
        "search_sessions",
        "since",
        ToolParameterConcept::SessionActivityWindow,
    ),
    (
        "search_sessions",
        "until",
        ToolParameterConcept::SessionActivityWindow,
    ),
    (
        "search_sessions",
        "when",
        ToolParameterConcept::SessionActivityPeriod,
    ),
    (
        "run_skill_capability",
        "since",
        ToolParameterConcept::MessageTimestampWindow,
    ),
    (
        "run_skill_capability",
        "until",
        ToolParameterConcept::MessageTimestampWindow,
    ),
    (
        "run_skill_capability",
        "when",
        ToolParameterConcept::MessageTimestampPeriod,
    ),
    (
        "run_skill_capability",
        "all_results",
        ToolParameterConcept::ReturnEveryMatch,
    ),
    (
        "list_sessions",
        "path_prefix",
        ToolParameterConcept::WorkspacePrefixFilter,
    ),
    (
        "search_sessions",
        "path_prefix",
        ToolParameterConcept::WorkspacePrefixFilter,
    ),
    (
        "run_skill_capability",
        "workspace_path_prefix",
        ToolParameterConcept::WorkspacePrefixFilter,
    ),
    (
        "get_session",
        "session_id",
        ToolParameterConcept::SessionSelector,
    ),
    (
        "get_resume_command",
        "session_id",
        ToolParameterConcept::SessionSelector,
    ),
    (
        "run_skill_capability",
        "session_id",
        ToolParameterConcept::SessionSelector,
    ),
    (
        "list_sessions",
        "include",
        ToolParameterConcept::OptionalPayloadGroups,
    ),
    (
        "search_sessions",
        "include",
        ToolParameterConcept::OptionalPayloadGroups,
    ),
    (
        "get_session",
        "include",
        ToolParameterConcept::OptionalPayloadGroups,
    ),
    (
        "get_session",
        "preview_chars",
        ToolParameterConcept::PreviewCharBudget,
    ),
    (
        "list_sessions",
        "preview_chars",
        ToolParameterConcept::PreviewCharBudget,
    ),
    (
        "search_sessions",
        "preview_chars",
        ToolParameterConcept::PreviewCharBudget,
    ),
    (
        "list_sessions",
        "exclude_session_ids",
        ToolParameterConcept::ExcludedSessionIds,
    ),
    (
        "search_sessions",
        "exclude_session_ids",
        ToolParameterConcept::ExcludedSessionIds,
    ),
    (
        "get_session",
        "lines_per_message",
        ToolParameterConcept::LineWindow,
    ),
    (
        "run_skill_capability",
        "field_view",
        ToolParameterConcept::FieldCharBudget,
    ),
    (
        "run_skill_capability",
        "match_view",
        ToolParameterConcept::MatchCharBudget,
    ),
    (
        "run_skill_capability",
        "detail",
        ToolParameterConcept::PresentationPreset,
    ),
    // Nested, and the one advertised parameter in this catalogue with no description at all.
    (
        "run_skill_capability",
        "definition.categories",
        ToolParameterConcept::SkillCategorySelector,
    ),
];

/// Apply the shared concept descriptions to one tool's advertised properties.
///
/// Called before `project_message_search_spec`, so where a `search_messages` field also names a
/// concept the registry still writes the final text. One owner per string, and the more specific
/// owner wins.
fn apply_every_shared_concept_description(response: &mut Value) {
    let Some(tools) = response
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for tool in tools {
        let Some(name) = tool["name"].as_str().map(str::to_owned) else {
            continue;
        };
        if let Some(properties) = tool
            .get_mut("inputSchema")
            .and_then(|schema| schema.get_mut("properties"))
            .and_then(Value::as_object_mut)
        {
            apply_shared_concept_descriptions(&name, properties);
        }
    }
}

fn apply_shared_concept_descriptions(
    tool_name: &str,
    properties: &mut serde_json::Map<String, Value>,
) {
    for (tool, field, concept) in TOOL_FIELD_CONCEPT {
        if *tool != tool_name {
            continue;
        }
        // A dotted path reaches a nested parameter. `definition.categories` is the reason this
        // exists: it is the one advertised parameter in the catalogue with no description at
        // all, and it is nested inside an object rather than sitting at the root.
        let mut cursor = Some(&mut *properties);
        let segments: Vec<&str> = field.split('.').collect();
        for (index, segment) in segments.iter().enumerate() {
            let Some(current) = cursor else { break };
            let Some(schema) = current.get_mut(*segment).and_then(Value::as_object_mut) else {
                break;
            };
            if index + 1 == segments.len() {
                schema.insert(
                    "description".to_owned(),
                    Value::String(concept_description(*concept).to_owned()),
                );
                break;
            }
            cursor = schema.get_mut("properties").and_then(Value::as_object_mut);
        }
    }
}

/// The `$defs` name for the one variant `field_view` and `match_view` genuinely share.
const CHAR_BUDGET_DEF: &str = "CharBudget";

/// Name the one bounded variant both view parameters share, and reference it from each.
///
/// Read the guard before widening this. `field_view` and `match_view` look like the same shape
/// and are not: their bounded `max_chars` variants are byte-identical, but their unbounded ones
/// differ, `no_char_limit` against `minimal_span`. Extracting the whole view saves more and
/// silently changes which values `match_view` accepts, which is the same class of regression a
/// cold read caught this work about to ship by hand. Only the shared variant is extractable, and
/// `extracting_the_shared_char_budget_keeps_each_view_s_own_variants` asserts each view still
/// accepts exactly its own two.
fn extract_shared_char_budget_variant(schema: &mut Value) {
    let Some(root) = schema.as_object_mut() else {
        return;
    };
    let Some(properties) = root.get("properties").and_then(Value::as_object).cloned() else {
        return;
    };
    // Find the variant the two views agree on, byte for byte, rather than assuming which it is.
    let bounded = ["field_view", "match_view"]
        .iter()
        .filter_map(|view| properties.get(*view)?.get("oneOf")?.as_array())
        .map(|variants| {
            variants
                .iter()
                .filter(|variant| variant["properties"].get("max_chars").is_some())
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let [field_bounded, match_bounded] = bounded.as_slice() else {
        return;
    };
    if field_bounded.len() != 1 || field_bounded != match_bounded {
        return;
    }
    let shared = field_bounded[0].clone();

    let reference = json!({ "$ref": format!("#/$defs/{CHAR_BUDGET_DEF}") });
    for view in ["field_view", "match_view"] {
        let Some(variants) = root
            .get_mut("properties")
            .and_then(|properties| properties.get_mut(view))
            .and_then(|schema| schema.get_mut("oneOf"))
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        for variant in variants.iter_mut() {
            if *variant == shared {
                *variant = reference.clone();
            }
        }
    }
    root.entry("$defs")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("$defs is an object")
        .insert(CHAR_BUDGET_DEF.to_owned(), shared);
}

fn project_message_search_spec(config: &Config, mut schema: Value) -> Value {
    let specification = MessageService::message_search_spec_for_config(config, SearchSurface::Mcp)
        .expect("validated MCP configuration resolves a queryless message-search request");
    let configured = specification.configured_default();
    let properties = schema
        .get_mut("properties")
        .and_then(Value::as_object_mut)
        .expect("built-in message-search schema has properties");

    // Every canonical parameter must reach a field this schema advertises.
    //
    // This walk used to publish the mapping as `x-aise-parameters` on each property so a test
    // could read it back. No MCP client is specified to read a vendor key and none does, so the
    // mapping is wire bytes for the twelve registrations that forward schemas verbatim and
    // nothing at all for the model. The invariant is stronger held here than published:
    // `message_search_mcp_fields` is exhaustive over `MessageSearchParameter`, so a new
    // canonical parameter cannot compile until it names its wire fields, and this panic fires
    // the moment one names a field the schema does not advertise. Tests join the two sides
    // through that function rather than off the wire, and
    // `search_messages_schema_covers_every_canonical_parameter_and_mcp_property` also asserts
    // the reverse direction, which the vendor key never covered: no field is advertised that no
    // planner parameter owns.
    for parameter in specification.registry().parameters() {
        for field in message_search_mcp_fields(parameter.parameter()) {
            assert!(
                properties.contains_key(*field),
                "canonical parameter {:?} maps to missing MCP field {field}",
                parameter.parameter()
            );
        }
    }

    let context = configured.context();
    set_schema_default(
        properties,
        "context_before",
        json!(context.messages_before()),
    );
    set_schema_default(properties, "context_after", json!(context.messages_after()));
    let context_schema = properties["context"]
        .as_object_mut()
        .expect("context schema is an object");
    if context.messages_before() == context.messages_after() {
        context_schema.insert("default".to_owned(), json!(context.messages_before()));
    } else {
        context_schema.remove("default");
    }

    match configured.extent() {
        ResolvedRequestExtent::Page { limit, offset } => {
            set_schema_default(properties, "limit", json!(limit));
            set_schema_default(properties, "all_results", json!(false));
            set_schema_default(properties, "offset", json!(offset));
        }
        ResolvedRequestExtent::AllResults { offset } => {
            properties["limit"]
                .as_object_mut()
                .expect("limit schema is an object")
                .remove("default");
            set_schema_default(properties, "all_results", json!(true));
            set_schema_default(properties, "offset", json!(offset));
        }
    }
    set_schema_default(
        properties,
        "lines_per_message",
        json!(configured.presentation().lines_per_message()),
    );
    set_schema_default(
        properties,
        "field_view",
        serde_json::to_value(configured.presentation().field_view())
            .expect("field-view default serializes"),
    );
    set_schema_default(
        properties,
        "match_view",
        serde_json::to_value(configured.presentation().match_view())
            .expect("match-view default serializes"),
    );
    set_schema_default(
        properties,
        "include",
        serde_json::to_value(configured.include()).expect("include default serializes"),
    );
    set_schema_default(
        properties,
        "receipt_level",
        serde_json::to_value(configured.receipt_level()).expect("receipt default serializes"),
    );
    if let Some(match_window) = configured.match_window() {
        set_schema_default(
            properties,
            "match_window",
            serde_json::to_value(match_window).expect("match-window default serializes"),
        );
    }

    // Render every description from the registry that already holds the facts, last, so it reads
    // the defaults resolved above rather than raw configuration.
    //
    // The schema literal this projects onto carries structure -- type, enum, items, required --
    // and no prose. Ninety-four hand-typed description literals used to carry an inflated copy
    // of what `selects`, `domain` and `omission` already say, which is why the emitted
    // descriptions measured 6,820 bytes against the registry's 1,770 bytes of the same facts.
    // Generating them leaves one source, so the two cannot disagree: a transcription error like
    // writing `no_char_limit` over `match_view`'s `minimal_span` becomes unrepresentable rather
    // than something a review has to catch.
    for (field, parameter, role) in MESSAGE_SEARCH_MCP_FIELDS {
        // `purpose` already has a generator, and a better-informed one: `purpose_input_schema`
        // names the bundles this deployment actually configured, which the registry cannot know
        // because it describes the parameter rather than the operator's configuration. Rendering
        // over it would replace a list of real choices with the generic sentence.
        if *field == GENERATED_ELSEWHERE_FIELD {
            continue;
        }
        let spec = specification
            .registry()
            .parameter(*parameter)
            .unwrap_or_else(|| panic!("{field} maps to unregistered parameter {parameter:?}"));
        let declared_default = properties
            .get(*field)
            .and_then(|schema| schema.get("default"))
            .cloned();
        let mut rendered = render_message_search_field_description(
            field,
            *role,
            spec,
            configured,
            declared_default.as_ref(),
        );
        // Every declared default reaches the text, whatever rendered it. `default` is one of the
        // six keywords Codex and VS Code both delete, so a default that lives only in the
        // keyword is a default no model is ever shown -- rule
        // `S9-state-every-default-in-the-text`. Appending here rather than in each arm means a
        // new role cannot forget it.
        if let Some(value) = declared_default.as_ref() {
            if !rendered.contains("omit for") {
                rendered.push_str(&format!(" Omit for {}.", render_default_value(value)));
            }
        }
        set_schema_description(properties, field, rendered);
    }

    extract_shared_char_budget_variant(&mut schema);

    // The nine conflict rules used to travel here as `x-aise-specification`, where no client
    // reads them and no model is shown them: measured, all nine reached the model nowhere. They
    // are not dropped, they change channel. `message_search_tool_description` renders
    // `registry().rule_descriptors()` verbatim into `tool.description`, which every client
    // forwards intact and Codex never strips, and
    // `search_messages_description_publishes_every_conflict_rule_verbatim` asserts substring
    // containment there so the published text cannot drift from the validator's.
    schema
}

/// The `tools[]` array this server advertises, without the JSON-RPC envelope around it.
///
/// Callers that measure the catalogue want the array; callers that measure transport want the
/// message. Those are two stages that differ by the wrapper, and reporting one figure for the
/// other is how a wire cost gets read as a model-context cost, so the two have separate accessors.
pub fn advertised_tools(config: &Config) -> Value {
    handle_tools_list(None, config)["result"]["tools"].clone()
}

/// One `search_messages` response measured at each stage it passes through on the way out.
///
/// The three response stages cannot be read off a `tools/list` capture, and reporting them as
/// zero would read as a saving rather than as an absence. So the ledger measures a real one.
///
/// The corpus is synthetic and built here rather than read from the configured index, for the
/// same reason the rest of the budget command never opens the index: a measurement that depends
/// on whatever a machine happens to have indexed is not comparable between two runs, let alone
/// between two machines. `fixture` names the corpus so a figure can be traced to the input that
/// produced it.
pub(crate) struct MeasuredResponse {
    pub fixture: &'static str,
    pub tool: &'static str,
    pub arguments: Value,
    pub returned: usize,
    /// `structuredContent` alone: the product payload, before any protocol wrapper.
    pub canonical_chars: usize,
    /// The serialized `CallToolResult`: the artifact the configured ceiling is enforced against.
    pub call_tool_result_chars: usize,
    /// The whole JSON-RPC message: transport cost, which the dispatcher does not own.
    pub jsonrpc_chars: usize,
}

/// The synthetic corpus the response stages are measured against, named so figures are traceable.
const LEDGER_FIXTURE: &str = "three-message-single-session-synthetic-v1";

/// Build the corpus, run one real search, and measure it through the real serializers.
///
/// Every stage is produced by the function that produces it in production -- the canonical
/// document by the shared response owner, the wrapper by `tool_response_to_rmcp`, the envelope by
/// the same `json!` shape the transport writes -- so a reshape that changes response size shows
/// up here instead of being described by a parallel serializer that drifted.
pub(crate) fn measure_representative_response(config: &Config) -> Result<MeasuredResponse, String> {
    use crate::models::{Message, MessageKind};
    use crate::util::minimal_record;

    // `tempfile` is a dev-dependency, and a diagnostic command is not worth promoting it to a
    // runtime one. The directory is removed below whether the measurement succeeds or fails.
    //
    // The counter matters: a process-unique name is not call-unique, and two concurrent callers
    // sharing one path had the first one's cleanup delete the second one's open database
    // mid-measurement, which surfaced as a bare SQLite I/O error rather than as anything naming
    // the collision.
    static LEDGER_FIXTURE_SEQUENCE: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    let root = std::env::temp_dir().join(format!(
        "aise-ledger-fixture-{}-{}-{LEDGER_FIXTURE}",
        std::process::id(),
        LEDGER_FIXTURE_SEQUENCE.fetch_add(1, Ordering::Relaxed),
    ));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("create ledger fixture root: {error}"))?;

    let measured = (|| -> Result<MeasuredResponse, String> {
        let db = Db::open(&root.join("index.db"))
            .map_err(|error| format!("open ledger fixture index: {error:#}"))?;
        let mut parsed = minimal_record(
            Provider::Claude,
            std::path::Path::new("/ledger/fixture.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:ledger-fixture".to_string();
        parsed.session.provider_session_id = "ledger-fixture".to_string();
        parsed.session.cwd = Some("/ledger/project".to_string());
        parsed.session.repo_root = Some("/ledger/project".to_string());
        parsed.session.title = Some("Ledger fixture session".to_string());
        parsed.session.message_count = Some(3);
        let message = |seq: i64, role: Role, content: &str| Message {
            seq,
            role,
            ts: None,
            tool_name: None,
            kind: MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: content.to_string(),
            provenance: Default::default(),
        };
        parsed.messages = vec![
            message(0, Role::User, "alpha ledger sample request"),
            message(1, Role::Assistant, "beta ledger sample response"),
            message(2, Role::User, "gamma ledger sample follow-up"),
        ];
        db.upsert_session(&parsed, 0, 0)
            .map_err(|error| format!("index ledger fixture: {error:#}"))?;

        let arguments = json!({ "query": "ledger", "index_refresh": "existing-only" });
        // The production dispatcher, so the measured bytes are the bytes a real call produces.
        let response = dispatch_tool_cancellable("search_messages", &arguments, config, &db, None)?;
        let structured = response.structured_content.clone().ok_or_else(|| {
            "the ledger fixture response carried no structuredContent".to_string()
        })?;
        let returned = structured["page"]["returned"].as_u64().unwrap_or_default() as usize;

        let call_tool_result = tool_response_to_rmcp(response);
        let call_tool_result_chars = serde_json::to_string(&call_tool_result)
            .map_err(|error| format!("serialize ledger CallToolResult: {error}"))?
            .chars()
            .count();
        let envelope = json!({ "jsonrpc": "2.0", "id": 2, "result": call_tool_result });
        let jsonrpc_chars = serde_json::to_string(&envelope)
            .map_err(|error| format!("serialize ledger JSON-RPC response: {error}"))?
            .chars()
            .count();

        Ok(MeasuredResponse {
            fixture: LEDGER_FIXTURE,
            tool: "search_messages",
            arguments,
            returned,
            canonical_chars: serde_json::to_string(&structured)
                .map_err(|error| format!("serialize ledger canonical result: {error}"))?
                .chars()
                .count(),
            call_tool_result_chars,
            jsonrpc_chars,
        })
    })();

    let _ = std::fs::remove_dir_all(&root);
    measured
}

fn handle_tools_list(id: Option<Value>, config: &Config) -> Value {
    let provider_values: Vec<_> = crate::source::PROVIDERS
        .into_iter()
        .map(|provider| provider.as_str())
        .collect();
    let provider_summary = crate::source::PROVIDERS
        .into_iter()
        .map(|provider| {
            format!(
                "{} (provider={})",
                provider.display_name(),
                provider.as_str()
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    // The slugs are the `enum`, which every measured client preserves; what a token cannot carry
    // is which product each slug names, so the mapping stays and the sentence around it goes.
    let provider_filter_description =
        format!("One session source; omit for all eight. {provider_summary}.");
    let message_provider_set_description = format!(
        "Only messages from these session sources; omit providers to include all eight. An empty \
         array is rejected, and duplicate values are normalized defensively by the service. \
         Accepted values: {provider_summary}."
    );
    let native_resume_summary = crate::source::PROVIDERS
        .into_iter()
        .filter(|provider| provider.supports_native_resume())
        .map(Provider::display_name)
        .collect::<Vec<_>>()
        .join(", ");
    let fallback_resume_summary = crate::source::PROVIDERS
        .into_iter()
        .filter(|provider| !provider.supports_native_resume())
        .map(Provider::display_name)
        .collect::<Vec<_>>()
        .join(", ");
    let schema_summary = sql_query::schema_summary_path(
        &config.db_path(),
        config.index.busy_timeout_ms,
        config.mcp.internal.schema_summary_tables,
        config.mcp.internal.schema_summary_columns,
    )
    .unwrap_or_else(|_| {
        "Schema unavailable until the aise index database exists; call query_session_index with no sql after indexing to inspect live AI session-history schema objects.".to_string()
    });
    let schema_summary = schema_summary.trim_end_matches(['.', ' ']);
    let query_session_index_description = format!(
        "Expert read-only SQL over the SQLite index for {provider_summary}. Prefer search_messages for content or regex search because it uses the FTS/trigram planner and returns context. Bounded live schema summary: {schema_summary}. Column values are provider spellings, so messages.tool_name is namespaced in Claude (mcp__server__tool) and a leaf name in Codex: match it with search_messages tool_name_contains, because one spelling drops the other provider silently. For how many messages contain a term, query messages_vocab(term, doc, cnt), which the listing omits as index machinery: doc counts messages and cnt occurrences, terms are case-folded, and `where term >= 'x' and term < 'y'` reads the index order instead of scanning. search_messages pages and reports no total. Omit sql to list schema objects; use schema_table for one table's columns and the note on each column that reads this way; pass sql only for one row-returning SELECT/WITH statement."
    );
    let mut response = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "search_sessions",
                    "annotations": read_only_tool_annotations(),
                    "outputSchema": search_sessions_output_schema(),
                    "description": format!("Search sessions from {provider_summary} by keyword, ranked by relevance. Read a result with get_session, reopen it with get_resume_command, or drill into turns with search_messages."),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Keywords, a phrase, or a code snippet to find in session titles and content."
                            },
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "path_prefix": {
                                "type": "string",
                                "description": "Only sessions whose working directory, git repo, or transcript path starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory."
                            },
                            "exclude_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude sessions whose working directory, git repo, or transcript path starts with any of these paths. Applied before limit. Omit for no path exclusions." },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" }, "description": "Exclude exact session IDs. Applied before limit. Omit for no session exclusions." },
                            "session_kinds": session_kinds_schema(),
                            "parent_session_id": parent_session_id_schema(),
                            "since": {
                                "type": "string",
                                "description": "Lower bound on session activity: last updated at or after this, or created at or after it when a session has no update time. Not a creation filter — a session started months ago and continued today is inside '7d'. Calendar/relative periods use UTC; an exact RFC 3339 timestamp honors Z or its explicit offset and preserves fractional seconds. Examples: '2026-01-15', '2026-01' (whole month), '202X' (whole decade), '7d' (last 7 days), 'yesterday', '2026-01-15T14:30:25.123Z'. Default: no lower bound."
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper bound on the same session-activity timestamp, inclusive. Same precision and timezone rules as since. Default: no upper bound."
                            },
                            "when": {
                                "type": "string",
                                "description": "Single UTC period used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. An exact RFC 3339 value selects that instant at its stated precision. Do not combine with since/until."
                            },
                            "limit": {
                                "type": "integer", "minimum": 0,
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Accepts a positive count or 0.", config.mcp.search_sessions_limit),
                                "default": config.mcp.search_sessions_limit
                            },
                            "offset": {
                                "type": "integer", "minimum": 0,
                                "description": "Skip this many higher-ranked matches before returning the page. Ranking is deterministic for one fixed index (score descending, updated_at descending, id ascending), but the index is not snapshotted and can change between calls. Work and retained top-K state scale with offset + limit. Default 0.",
                                "default": 0
                            },
                            "include": raw_metadata_include_schema(),
                            "preview_chars": session_preview_chars_schema()
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }
                },
                {
                    "name": "get_session",
                    "annotations": read_only_tool_annotations(),
                    "description": format!("Return one session from {provider_summary} by ID or unique prefix. Use summary=true for compact evidence, transcript_lines=N for transcript text (0 returns all lines), message_seq=N with context for one turn, or seq_from/seq_to for an absolute message range. To read more, continue from the next seq range (seq_from = last returned seq + 1) rather than re-requesting with a larger transcript_lines, which re-sends what you already received. Default returns {} transcript lines.", transcript_lines_default_label(config.mcp.get_session_transcript_lines)),
                    "outputSchema": get_session_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix, e.g. 'claude:abc123' or 'abc123'."
                            },
                            "summary": {
                                "type": "boolean",
                                "description": "Return compact session summary/evidence: stored opening purpose plus selected user intent, tool activity previews, refs, aggregate changed-file summaries, provenance, and bounded follow-up commands. summary_items controls message-derived evidence and the shared aggregate cap; truncated_evidence names categories with additional indexed entries. Mutually exclusive with transcript_lines and message_seq. Default false, which returns transcript lines instead.",
                                "default": false
                            },
                            "summary_items": { "type": "integer", "description": format!("With summary=true, select aggregate evidence records: positive=first, negative=last, 0=all (default {}). Message-derived records are displayed chronologically; changed_files remains an aggregate ordered by path and edit count. This changes presentation only; use bounded search_messages pages for deterministic non-overlapping detail retrieval.", config.mcp.summary_items), "default": config.mcp.summary_items },
                            "include": { "type": "array", "items": { "type": "string", "enum": ["time_profile"] }, "description": "Optional bounded summary sections (default none). Currently supports time_profile. Requires summary=true.", "default": [] },
                            "transcript_lines": {
                                "type": "integer",
                                "description": format!("Return transcript lines: positive=head, negative=tail, 0=entire transcript and may be very large. Bound this when skimming many sessions: a negative tail shows how a session ended, a positive head shows how it started, and 0 is for complete capture only. To pinpoint one turn, use search_messages and pass its message_seq here instead of reading a large window. Mutually exclusive with summary and message_seq. Default when no output selector is provided: {}.", config.mcp.get_session_transcript_lines),
                                "default": config.mcp.get_session_transcript_lines
                            },
                            "message_seq": {
                                "type": "integer", "minimum": 0,
                                "description": "Message sequence number copied from search_messages `results[].message_ref.message_seq`. Returns a focused message-context result instead of transcript lines."
                            },
                            "seq_from": {
                                "type": "integer", "minimum": 0,
                                "description": "Lower inclusive message-sequence bound for an absolute range read of this session's messages. seq numbers are session-local, which this per-session tool already scopes. Pair with seq_to to read one session in non-overlapping chunks (e.g. 0..499, then 500..999) instead of re-reading a larger transcript_lines head/tail. Mutually exclusive with summary, transcript_lines, and message_seq."
                            },
                            "seq_to": {
                                "type": "integer", "minimum": 0,
                                "description": "Upper inclusive message-sequence bound for an absolute range read. See seq_from for non-overlapping chunked reads. Must be >= seq_from when both are given."
                            },
                            "context": {
                                "type": "integer", "minimum": 0,
                                "description": "When message_seq is provided, include this many turns before and after that message (default 0).",
                                "default": 0
                            },
                            "include_refs": {
                                "type": "boolean",
                                "description": "When message_seq is provided, include extracted URL-like references for each returned message (default false).",
                                "default": false
                            },
                            "preview_chars": { "type": "integer", "minimum": 1, "description": format!("Maximum characters per concise message/tool/ref preview in summary output and focused message context (default {}). Not used for transcript output.", config.mcp.preview_chars.max(1)), "default": config.mcp.preview_chars.max(1) },
                            "lines_per_message": {
                                "type": "integer",
                                "description": format!("With message_seq: limit each returned message's displayed content (positive keeps its first N lines, negative keeps its last N lines, 0 keeps complete content; default {}). This presentation window does not change context membership or reference extraction. Use it to keep long tool output around one turn skimmable. It bounds each returned message on its own; use transcript_lines to window a whole session transcript.", config.mcp.lines_per_message),
                                "default": config.mcp.lines_per_message
                            },
                            "response_format": {
                                "type": "string",
                                "enum": ["concise", "detailed"],
                                "description": "When message_seq is provided, concise (default) applies preview_chars after the per-message line window; detailed applies no additional character cap after that line window.",
                                "default": "concise"
                            }
                        },
                        "required": ["session_id"],
                        "additionalProperties": false
                    }
                },
                {
                    "name": "list_sessions",
                    "annotations": read_only_tool_annotations(),
                    "outputSchema": list_sessions_output_schema(),
                    "description": "List indexed sessions newest first. Use provider to select one named session source; use search_sessions for keywords.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "path_prefix": {
                                "type": "string",
                                "description": "Only sessions whose working directory, git repo, or transcript path starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory."
                            },
                            "exclude_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude sessions whose working directory, git repo, or transcript path starts with any of these paths. Applied before limit. Omit for no path exclusions." },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" }, "description": "Exclude exact session IDs. Applied before limit. Omit for no session exclusions." },
                            "session_kinds": session_kinds_schema(),
                            "parent_session_id": parent_session_id_schema(),
                            "since": {
                                "type": "string",
                                "description": "Lower bound on session activity: last updated at or after this, or created at or after it when a session has no update time. Not a creation filter — a session started months ago and continued today is inside '7d'. Calendar/relative periods use UTC; an exact RFC 3339 timestamp honors Z or its explicit offset and preserves fractional seconds. Examples: '2026-01-15', '202X' (whole decade), '7d' (last 7 days), 'yesterday', '2026-01-15T14:30:25.123Z'. Default: no lower bound."
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper bound on the same session-activity timestamp, inclusive. Same precision and timezone rules as since. Default: no upper bound."
                            },
                            "when": {
                                "type": "string",
                                "description": "Single UTC period used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. An exact RFC 3339 value selects that instant at its stated precision. Do not combine with since/until."
                            },
                            "limit": {
                                "type": "integer", "minimum": 0,
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Accepts a positive count or 0.", config.mcp.list_sessions_limit),
                                "default": config.mcp.list_sessions_limit
                            },
                            "offset": {
                                "type": "integer", "minimum": 0,
                                "description": "Number of newest-first sessions to skip before returning this page. Default 0.",
                                "default": 0
                            },
                            "include": raw_metadata_include_schema(),
                            "preview_chars": session_preview_chars_schema()
                        },
                        "additionalProperties": false
                    }
                },
                {
                    "name": "get_resume_command",
                    "annotations": read_only_tool_annotations(),
                    "outputSchema": get_resume_command_output_schema(),
                    "description": format!("Return a copy-pastable POSIX-shell rendering of the native resume arguments for {native_resume_summary}. This text is not PowerShell or cmd.exe syntax. {fallback_resume_summary} cannot be resumed; the tool returns an error with exact `aise show` and `aise export` fallback commands."),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix, e.g. 'claude:abc123' or 'abc123'."
                            }
                        },
                        "required": ["session_id"],
                        "additionalProperties": false
                    }
                },
                {
                    "name": "search_messages",
                    "annotations": read_only_tool_annotations(),
                    "description": message_search_tool_description(config),
                    "outputSchema": search_messages_output_schema(),
                    "inputSchema": project_message_search_spec(config, json!({
                        "type": "object",
                        "properties": {
                            "query": { "type": "string" },
                            "query_mode": { "type": "string", "enum": ["literal", "regex", "fuzzy"], "default": "literal" },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"] },
                            "kind": { "type": "string", "enum": message_kind_values() },
                            "field": { "type": "string", "enum": ["content", "tool_name", "tool_argument"], "default": "content" },
                            "argument_path": { "type": "string" },
                            "providers": provider_set_schema(
                                &provider_values,
                                &message_provider_set_description
                            ),
                            "tool_name_contains": { "type": "string" },
                            "session_id": { "type": "string" },
                            "workspace_path_prefix": { "type": "string" },
                            "transcript_path_prefix": { "type": "string" },
                            "exclude_workspace_path_prefixes": { "type": "array", "items": { "type": "string" } },
                            "exclude_transcript_path_prefixes": { "type": "array", "items": { "type": "string" } },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" } },
                            "seq_from": { "type": "integer", "minimum": 0 },
                            "seq_to": { "type": "integer", "minimum": 0 },
                            "since": { "type": "string" },
                            "until": { "type": "string" },
                            "when": { "type": "string" },
                            "include_compaction": { "type": "boolean", "default": true },
                            "kinds": { "type": "array", "items": { "type": "string", "enum": message_kind_values() } },
                            "match_window": { "type": "string", "enum": ["earliest", "latest"] },
                            "context": { "type": "integer", "minimum": 0, "default": 0 },
                            "context_before": { "type": "integer", "minimum": 0 },
                            "context_after": { "type": "integer", "minimum": 0 },
                            "lines_per_message": { "type": "integer", "default": config.mcp.lines_per_message },
                            "detail": { "type": "string", "enum": ["compact", "full"] },
                            "field_view": {
                                "default": {
                                    "kind": "max_chars",
                                    "max_chars": config.mcp.preview_chars.max(1)
                                },
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": { "kind": { "enum": ["no_char_limit"] } },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "enum": ["max_chars"] },
                                            "max_chars": { "type": "integer", "minimum": 1 }
                                        },
                                        "required": ["kind", "max_chars"],
                                        "additionalProperties": false
                                    }
                                ]
                            },
                            "match_view": {
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": { "kind": { "enum": ["minimal_span"] } },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "enum": ["max_chars"] },
                                            "max_chars": { "type": "integer", "minimum": 1 }
                                        },
                                        "required": ["kind", "max_chars"],
                                        "additionalProperties": false
                                    }
                                ]
                            },
                            "include": {
                                "type": "array",
                                "uniqueItems": true,
                                "items": { "type": "string", "enum": ["normalized_session_metadata", "parsed_references", "raw_provider_metadata", "runtime_diagnostics"] },
                                "description": "Optional payload groups: normalized_session_metadata adds deduplicated normalized session fields; parsed_references adds per-result parsed references; raw_provider_metadata adds verbatim provider metadata; runtime_diagnostics adds package/config/schema identity. Omit to use the MCP default; an explicit empty array requests only the semantic core. A supplied set replaces defaults."
                            },
                            "purpose": purpose_input_schema(config),
                            "purpose_version": { "type": "integer", "minimum": 1 },
                            "receipt_level": { "type": "string", "enum": ["none", "summary", "full"] },
                            "limit": { "type": "integer", "minimum": 1, "default": config.mcp.search_messages_limit.max(1) },
                            "all_results": { "type": "boolean", "default": false },
                            "offset": { "type": "integer", "minimum": 0, "default": 0 }
                        },
                        "additionalProperties": false
                    }))
                },
                {
                    "name": "run_skill_capability",
                    "annotations": read_only_tool_annotations(),
                    "description": format!("Execute deterministic message-classification rules under one selected Aise skill package across {provider_summary}. By default Aise reads the package's capability.toml; definition can supply typed categories directly for one call. The MCP client or AI harness, not Aise, loads and follows SKILL.md. Select corrections or another catalog package by name, or pass a package path authorized by [skills].search_paths. Selected packaged and direct capability definitions share a 1 MiB aggregate parsing safety ceiling; exceeding it returns byte counts and guidance rather than truncating rules or results. Defaults to user messages in user-started sessions. Returns the resolved package, capability and policy receipts, source-appropriate digests, matches, and pagination. For corrections, this is equivalent to `aise skills corrections --format json`."),
                    "outputSchema": run_skill_capability_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "skill": skill_selector_input_schema(),
                            "definition": {
                                "type": "object",
                                "description": "Direct typed message-classification rules for this call. These categories replace only the primary selected skill's adjacent capability.toml rules; the selected skill still owns identity, version, instructions, and path authorization.",
                                "properties": {
                                    "categories": {
                                        "type": "array",
                                        "minItems": 1,
                                        "items": {
                                            "type": "object",
                                            "properties": {
                                                "name": { "type": "string", "minLength": 1 },
                                                "patterns": {
                                                    "type": "array",
                                                    "minItems": 1,
                                                    "items": { "type": "string", "minLength": 1 }
                                                }
                                            },
                                            "required": ["name", "patterns"],
                                            "additionalProperties": false
                                        }
                                    }
                                },
                                "required": ["categories"],
                                "additionalProperties": false
                            },
                            "detail": { "type": "string", "enum": ["compact", "full"], "description": "Presentation preset only. compact returns bounded field and match views; full returns the complete message in field_view while retaining a bounded match-centered view. It never changes classification, ordering, result count, pagination, or digests. Conflicts with field_view and match_view." },
                            "field_view": {
                                "description": format!("Returned-message boundary view budget after full-text classification. no_char_limit returns the complete message; max_chars retains at most that many Unicode scalar characters. The configured MCP default is max_chars={}. Extent metadata and message_ref make every bounded result explicit and exactly recoverable.", config.mcp.preview_chars.max(1)),
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": { "kind": { "enum": ["no_char_limit"] } },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "enum": ["max_chars"] },
                                            "max_chars": { "type": "integer", "minimum": 1 }
                                        },
                                        "required": ["kind", "max_chars"],
                                        "additionalProperties": false
                                    }
                                ]
                            },
                            "match_view": {
                                "description": "Independent match-centered view after full-text classification. minimal_span returns the complete regex match; max_chars adds surrounding message text without changing the match coordinates or classification.",
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": { "kind": { "enum": ["minimal_span"] } },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "enum": ["max_chars"] },
                                            "max_chars": { "type": "integer", "minimum": 1 }
                                        },
                                        "required": ["kind", "max_chars"],
                                        "additionalProperties": false
                                    }
                                ]
                            },
                            "additional_skills": { "type": "array", "uniqueItems": true, "items": skill_selector_input_schema(), "description": "Additional skill packages whose rules are evaluated after the primary package. Every package must declare the same capability type. This does not load or follow their SKILL.md instructions." },
                            "session_kinds": { "type": "array", "items": { "type": "string", "enum": session_kind_values() }, "description": "Which session classes to scan. Omit for user-started sessions only: in a spawned subagent run, 'user' rows contain the calling agent's delegation prompt rather than text a person entered. Pass [\"user\", \"subagent\"] to scan both. This default differs from search_messages and list_sessions, which return both classes." },
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "session_id": { "type": "string", "description": "Exact session ID or unique prefix. Use to scope the capability run to one session found by search_sessions." },
                            "workspace_path_prefix": { "type": "string", "description": "Only sessions whose working directory or repository root starts with this path. Use to scope the capability run to one project." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. Calendar/relative periods use UTC. Examples: '2026-01-15', '7d', 'yesterday'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound, inclusive: messages at or before this. Same rules as since. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single UTC period used as both bounds, e.g. '2026-01', '7d', 'yesterday'. Do not combine with since/until." },
                            "limit": { "type": "integer", "minimum": 1, "description": format!("Positive page size. Omit to use the configured MCP default of {}. Use all_results for every match.", config.mcp.run_message_classification_limit), "default": config.mcp.run_message_classification_limit },
                            "all_results": { "type": "boolean", "description": "Return every match rather than one page. Defaults to false; conflicts with limit. Each match carries a whole user message, so prefer paging when the range is wide.", "default": false },
                            "offset": { "type": "integer", "minimum": 0, "description": "Skip this many matches before returning, newest first, to page through results (default 0). Accepts a positive count or 0.", "default": 0 }
                        },
                        "required": ["skill"],
                        "additionalProperties": false
                    }
                },
                {
                    "name": "get_index_status",
                    "annotations": read_only_tool_annotations(),
                    "description": format!("Return index and parser status for {provider_summary}: current and stale session counts, parse warnings, discoverable sessions that can be reindexed, retained sessions whose source files are unavailable, actionable automatic index-update status when work is running or requires attention, and applicable repair commands. Equivalent to `aise doctor --format json`."),
                    "outputSchema": get_index_status_output_schema(),
                    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
                },
                {
                    "name": "query_session_index",
                    "annotations": read_only_tool_annotations(),
                    "description": query_session_index_description,
                    "outputSchema": query_session_index_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "sql": { "type": "string", "description": "Exactly one raw read-only SQL statement returning rows from the local AI session-history index. Omit sql to list session-history schema objects. Prefer search_messages for accelerated content or regex search with context. Writes, ATTACH/DETACH, unsafe PRAGMAs, and multiple statements are rejected." },
                            "schema_table": { "type": "string", "description": "Optional table/view name for column details in the AI session-history index, such as sessions, messages, or file_edits. Use instead of sql." },
                            "include_internal": { "type": "boolean", "description": "When sql is omitted, include SQLite/FTS shadow tables and internal indexes for the session-history database (default false).", "default": false },
                            "limit": { "type": "integer", "minimum": 0, "description": format!("Maximum rows to return after the SQL statement runs (default {}). 0 means unlimited; prefer adding LIMIT in SQL for expensive queries. Accepts a positive count or 0.", config.db.query_limit), "default": config.db.query_limit },
                            "offset": { "type": "integer", "minimum": 0, "description": "Skip this many rows after the SQL statement runs (default 0). Prefer SQL LIMIT/OFFSET for expensive queries. Accepts a positive count or 0.", "default": 0 },
                            "timeout_ms": { "type": "integer", "minimum": 0, "description": format!("MCP-only raw-SQL availability guard in milliseconds (default {}). 0 disables interruption. This is independent of native CLI/Rust SQL defaults and does not apply to indexed search tools.", config.mcp.query_timeout_ms), "default": config.mcp.query_timeout_ms },
                            "max_cell_chars": { "type": "integer", "minimum": 0, "description": format!("Maximum characters per string cell in the JSON response. 0 disables cell truncation. Default {}.", config.mcp.query_max_cell_chars), "default": config.mcp.query_max_cell_chars }
                        },
                        "additionalProperties": false
                    }
                }
            ]
        }
    });
    apply_every_shared_concept_description(&mut response);
    add_index_refresh_controls(&mut response);
    add_not_ready_output_alternatives(&mut response);
    response
}

fn dispatch_tool_cancellable(
    tool_name: &str,
    args: &Value,
    config: &Config,
    db: &Db,
    cancellation: Option<&QueryCancellation>,
) -> Result<ToolResponse, String> {
    if cancellation.is_some_and(QueryCancellation::is_cancelled) {
        return Err("MCP tool call was cancelled".to_string());
    }
    match tool_name {
        "search_sessions" => tool_search_sessions(args, config, db),
        "get_session" => tool_get_session(args, config, db),
        "list_sessions" => tool_list_sessions(args, config, db),
        "get_resume_command" => tool_get_resume_command(args, db),
        "search_messages" => tool_search_messages_cancellable(
            args,
            config,
            db,
            cancellation.map(QueryCancellation::flag),
        ),
        "run_skill_capability" => tool_run_skill_capability(args, config, db),
        "get_index_status" => crate::diagnostics::collect(config, db)
            .map_err(|error| format!("{error:#}"))
            .and_then(|status| serde_json::to_value(status).map_err(|error| format!("{error:#}")))
            .and_then(ToolResponse::structured),
        "query_session_index" => tool_query_session_index_cancellable(args, config, cancellation),
        // Derive the served names from the advertised list rather than restating them, so this
        // recovery hint can never drift from what tools/list actually publishes.
        _ => Err(unknown_tool_message(
            tool_name,
            &handle_tools_list(None, config)["result"]["tools"],
        )),
    }
}

fn is_schema_only_index_call(params: &Value) -> bool {
    if params.get("name").and_then(Value::as_str) != Some("query_session_index") {
        return false;
    }
    params
        .get("arguments")
        .and_then(|args| args.get("sql"))
        .and_then(Value::as_str)
        .is_none_or(|sql| sql.trim().is_empty())
}

#[derive(Debug)]
struct ToolResponse {
    text: String,
    structured_content: Option<Value>,
}

impl ToolResponse {
    fn structured(value: Value) -> Result<Self, String> {
        let text = serde_json::to_string_pretty(&value).map_err(|err| format!("{err:#}"))?;
        Ok(Self::structured_with_text(text, value))
    }

    fn structured_with_text(text: String, value: Value) -> Self {
        Self {
            text,
            structured_content: Some(value),
        }
    }
}

fn tool_not_ready_response(readiness: crate::models::IndexReadinessStatus) -> ToolResponse {
    let refresh = &readiness.refresh;
    let progress = match (refresh.files_processed, refresh.files_discovered) {
        (Some(processed), Some(discovered)) => {
            format!(" ({processed}/{discovered} files processed)")
        }
        _ => String::new(),
    };
    let detail = refresh
        .message
        .as_deref()
        .map(|message| format!(" {message}"))
        .unwrap_or_default();
    let text = format!(
        "Session history has no usable snapshot; refresh is {}{progress}.{detail} The requested operation was not run. Call get_index_status with no arguments, then retry this tool after snapshot availability becomes usable.",
        refresh.state.as_str()
    );
    ToolResponse::structured_with_text(
        text,
        json!({
            "operation_executed": false,
            "index_status": readiness,
            "next_call": {
                "tool": "get_index_status",
                "arguments": {}
            }
        }),
    )
}

#[cfg(test)]
impl std::ops::Deref for ToolResponse {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

fn transcript_lines_default_label(transcript_lines: i64) -> String {
    match transcript_lines.cmp(&0) {
        std::cmp::Ordering::Less => format!("the last {}", transcript_lines.unsigned_abs()),
        std::cmp::Ordering::Equal => "the entire transcript".to_string(),
        std::cmp::Ordering::Greater => format!("the first {transcript_lines}"),
    }
}

fn tool_search_sessions(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: query")?;
    let now = chrono::Utc::now();
    let mut filters = search_filters_from_args(args, config.mcp.search_sessions_limit, now)?;
    let offset = mcp_nonnegative_usize_arg(args, "offset", 0)?;
    let requested_limit = filters.limit;
    if requested_limit > 0 {
        filters.limit = offset
            .checked_add(requested_limit)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| {
                "search_sessions offset plus limit and look-ahead overflows".to_string()
            })?;
    }
    let repo = current_repo(config);
    let mut hits = CatalogService::new(db)
        .search_sessions(query, &filters, repo.as_deref(), &config.search.scoring)
        .map_err(|e| format!("{e:#}"))?;
    let page_end = offset
        .checked_add(requested_limit)
        .ok_or_else(|| "search_sessions offset plus limit overflows".to_string())?;
    let has_more = requested_limit > 0 && hits.len() > page_end;
    if offset > 0 {
        hits.drain(..offset.min(hits.len()));
    }
    if requested_limit > 0 {
        hits.truncate(requested_limit);
    }
    let next_offset = has_more
        .then(|| {
            offset
                .checked_add(hits.len())
                .ok_or_else(|| "search_sessions next_offset overflows".to_string())
        })
        .transpose()?;

    // Structured output mirrors `aise search --format json` (an array of flattened
    // SearchHit records) so MCP and CLI consumers see the same element shape; the text
    // stays a compact human-readable digest via structured_with_text.
    let mut hit_values = serde_json::to_value(&hits).map_err(|e| format!("{e:#}"))?;
    apply_raw_metadata_include(&mut hit_values, &parse_string_array(args, "include")?);
    apply_session_preview_chars(
        &mut hit_values,
        mcp_optional_positive_usize_arg(args, "preview_chars")?,
    );
    let structured = json!({
        "sessions": hit_values,
        "returned": hits.len(),
        "has_more": has_more,
        "next_offset": next_offset,
        "pagination": {
            "offset": offset,
            "order": "score-desc,updated-at-desc,id-asc",
            "consistency": "per-call",
        },
    });

    if hits.is_empty() {
        return Ok(ToolResponse::structured_with_text(
            "No sessions found matching the query.".to_string(),
            structured,
        ));
    }

    let mut out = String::new();
    for hit in &hits {
        let s = &hit.session;
        let title = s
            .title
            .as_deref()
            .map(|t| truncate_for_display(t, 120))
            .unwrap_or_else(|| "(untitled)".to_string());
        let cwd = s.cwd.as_deref().unwrap_or("-");
        let updated = s
            .updated_at
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "## {} [{}] (score: {})\n- ID: {}\n- Provider: {}\n- CWD: {}\n- Updated: {}\n- Match: {} — {}\n\n",
            title,
            s.provider,
            hit.score,
            s.id,
            s.provider,
            cwd,
            updated,
            hit.match_source,
            hit.match_snippet,
        ));
    }
    Ok(ToolResponse::structured_with_text(out, structured))
}

fn tool_get_session(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;
    let summary = mcp_bool_arg(args, "summary", false);
    let message_seq = args.get("message_seq").and_then(Value::as_i64);
    let transcript_lines = args.get("transcript_lines").and_then(Value::as_i64);
    // Absolute message-range read: an alternative to a bigger transcript_lines head/tail that lets
    // the caller advance seq_from = last seq + 1 for deterministic, non-overlapping chunks. seq
    // numbers are session-local, which this per-session tool already scopes.
    let seq_from = args.get("seq_from").and_then(Value::as_i64);
    let seq_to = args.get("seq_to").and_then(Value::as_i64);
    let has_range = seq_from.is_some() || seq_to.is_some();

    let selector_count = summary as usize
        + message_seq.is_some() as usize
        + transcript_lines.is_some() as usize
        + has_range as usize;
    if selector_count > 1 {
        return Err(
            "Use only one get_session output selector: summary, transcript_lines, message_seq, or seq_from/seq_to."
                .to_string(),
        );
    }
    if let (Some(from), Some(to)) = (seq_from, seq_to) {
        if from > to {
            return Err(format!(
                "seq_from must be <= seq_to, got {from} > {to}; \
                 swap the bounds or raise seq_to to at least {from}"
            ));
        }
    }

    if summary {
        let include = parse_string_array(args, "include")?;
        if let Some(unsupported) = include
            .iter()
            .find(|value| value.as_str() != "time_profile")
        {
            return Err(format!(
                "unsupported get_session include value: {unsupported}"
            ));
        }
        reject_non_default(
            args,
            "include_refs",
            json!(false),
            "include_refs only applies with message_seq; summary already includes reference evidence",
        )?;
        reject_non_default(
            args,
            "context",
            json!(0),
            "context only applies with message_seq; summary includes follow-up commands for larger windows",
        )?;
        reject_non_default(
            args,
            "response_format",
            json!("concise"),
            "response_format only applies with message_seq; summary always returns structured evidence with bounded previews",
        )?;
        reject_non_default(
            args,
            "lines_per_message",
            json!(config.mcp.lines_per_message),
            "lines_per_message only applies with message_seq; summary uses preview_chars for its bounded previews",
        )?;
        let mut options = inspection_options_from_args(args, config)?;
        options.include_time_profile = include.iter().any(|value| value == "time_profile");
        let inspection = CatalogService::new(db)
            .inspect(session_id, options)
            .map_err(|e| format!("{e:#}"))?;
        return serde_json::to_value(&inspection)
            .map_err(|e| format!("{e:#}"))
            .and_then(ToolResponse::structured);
    }

    if let Some(seq) = message_seq {
        let session = db
            .resolve_session_record(session_id)
            .map_err(|e| format!("{e:#}"))?;
        let context = mcp_nonnegative_i64_arg(args, "context", 0)?;
        let presentation = MessagePresentation::from_args(args, config)?;
        return message_window_value(&session, seq, context, &presentation, db)
            .and_then(ToolResponse::structured);
    }

    if has_range {
        reject_non_default(
            args,
            "context",
            json!(0),
            "context only applies with message_seq; a seq_from/seq_to range reads every message in [seq_from, seq_to]",
        )?;
        let session = db
            .resolve_session_record(session_id)
            .map_err(|e| format!("{e:#}"))?;
        let presentation = MessagePresentation::from_args(args, config)?;
        return message_range_value(&session, seq_from, seq_to, &presentation, db)
            .and_then(ToolResponse::structured);
    }
    reject_non_default(
        args,
        "summary_items",
        json!(config.mcp.summary_items),
        "summary_items only applies with summary=true",
    )?;
    reject_non_default(
        args,
        "include",
        json!([]),
        "include only applies with summary=true",
    )?;
    reject_non_default(
        args,
        "context",
        json!(0),
        "context only applies with message_seq; transcript output uses transcript_lines",
    )?;
    reject_non_default(
        args,
        "include_refs",
        json!(false),
        "include_refs only applies with message_seq; transcript output returns raw transcript lines",
    )?;
    reject_non_default(
        args,
        "preview_chars",
        json!(config.mcp.preview_chars.max(1)),
        "preview_chars only applies to summary output and focused message context",
    )?;
    reject_non_default(
        args,
        "response_format",
        json!("concise"),
        "response_format only applies with message_seq; transcript output uses transcript_lines",
    )?;
    reject_non_default(
        args,
        "lines_per_message",
        json!(config.mcp.lines_per_message),
        "lines_per_message caps each message and only applies with message_seq; transcript output windows the whole session with transcript_lines",
    )?;
    let selected_lines = transcript_lines.unwrap_or(config.mcp.get_session_transcript_lines);

    let full = db
        .resolve_session(session_id)
        .map_err(|e| format!("{e:#}"))?;
    let s = &full.session;

    let (transcript, returned_lines_label) =
        select_transcript_lines(&full.transcript_text, selected_lines);
    let total_lines = full.transcript_text.lines().count();
    let returned_lines = transcript.lines().count();
    let selected_edge = match selected_lines.cmp(&0) {
        std::cmp::Ordering::Less => "tail",
        std::cmp::Ordering::Equal => "all",
        std::cmp::Ordering::Greater => "head",
    };
    let complete = returned_lines == total_lines;

    let title = s.title.as_deref().unwrap_or("(untitled)");
    let cwd = s.cwd.as_deref().unwrap_or("-");
    let updated = s
        .updated_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());

    let text = format!(
        "# {title}\n\n- ID: {}\n- Provider: {}\n- Provider Session ID: {}\n- CWD: {cwd}\n- Updated: {updated}\n- Messages: {}\n- Transcript lines returned: {returned_lines_label}\n\n## Transcript\n\n{transcript}",
        s.id,
        s.provider,
        s.provider_session_id,
        s.message_count.unwrap_or(0),
    );
    Ok(ToolResponse::structured_with_text(
        text.clone(),
        json!({
            "session": session_record_meta_json(s, true),
            "transcript": {
                "text": transcript,
                "total_lines": total_lines,
                "lines_returned": returned_lines,
                "selected_edge": selected_edge,
                "complete": complete,
            },
            "rendered_text": text,
        }),
    ))
}

fn tool_list_sessions(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    let now = chrono::Utc::now();
    let mut filters = search_filters_from_args(args, config.mcp.list_sessions_limit, now)?;
    let offset = mcp_nonnegative_usize_arg(args, "offset", 0)?;
    let requested_limit = filters.limit;
    if requested_limit > 0 {
        filters.limit = requested_limit
            .checked_add(1)
            .ok_or_else(|| "list_sessions limit plus look-ahead overflows".to_string())?;
    }
    let mut sessions = CatalogService::new(db)
        .list_sessions_page(&filters, offset)
        .map_err(|e| format!("{e:#}"))?;
    let has_more = requested_limit > 0 && sessions.len() > requested_limit;
    if requested_limit > 0 {
        sessions.truncate(requested_limit);
    }
    let next_offset = if has_more {
        Some(
            offset
                .checked_add(requested_limit)
                .ok_or_else(|| "list_sessions next offset overflows".to_string())?,
        )
    } else {
        None
    };

    // Structured output mirrors `aise list --format json` (an array of session records).
    let mut session_values = serde_json::to_value(&sessions).map_err(|e| format!("{e:#}"))?;
    apply_raw_metadata_include(&mut session_values, &parse_string_array(args, "include")?);
    apply_session_preview_chars(
        &mut session_values,
        mcp_optional_positive_usize_arg(args, "preview_chars")?,
    );
    let structured = json!({
        "sessions": session_values,
        "returned": sessions.len(),
        "has_more": has_more,
        "next_offset": next_offset,
    });

    if sessions.is_empty() {
        return Ok(ToolResponse::structured_with_text(
            "No sessions found.".to_string(),
            structured,
        ));
    }

    let mut out = String::new();
    for s in &sessions {
        let title = s
            .title
            .as_deref()
            .map(|t| truncate_for_display(t, 120))
            .unwrap_or_else(|| "(untitled)".to_string());
        let cwd = s.cwd.as_deref().unwrap_or("-");
        let updated = s
            .updated_at
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".to_string());

        out.push_str(&format!(
            "- **{}** [{}] — {} | CWD: {} | ID: {}\n",
            title, s.provider, updated, cwd, s.id,
        ));
    }
    Ok(ToolResponse::structured_with_text(out, structured))
}

fn tool_get_resume_command(args: &Value, db: &Db) -> Result<ToolResponse, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;

    let session = db
        .resolve_session_record(session_id)
        .map_err(|e| format!("{e:#}"))?;
    let (command, cwd) = resume_plan(&session).map_err(|e| format!("{e:#}"))?;

    let cmd_str = render_posix_shell_command(&command).map_err(|error| format!("{error:#}"))?;
    // The text is the ready-to-run command; structured output names the resolved session
    // and working directory so a caller can resume programmatically without parsing prose.
    let (resume_command, cwd_value) = match cwd {
        Some(cwd) => {
            let change_dir = render_posix_shell_command(&["cd".to_string(), cwd.clone()])
                .map_err(|error| format!("{error:#}"))?;
            (format!("{change_dir} && {cmd_str}"), Value::String(cwd))
        }
        None => (cmd_str, Value::Null),
    };
    let structured = json!({
        "session_id": session.id,
        "resume_command": resume_command.clone(),
        "cwd": cwd_value,
    });
    Ok(ToolResponse::structured_with_text(
        resume_command,
        structured,
    ))
}

/// Run the deterministic capability declared by a selected skill package.
///
/// The MCP adapter authorizes explicit package paths and translates paging. Package discovery,
/// capability parsing, classification, and provenance remain in the shared analysis service.
fn parse_mcp_skill_selector(value: &Value) -> Result<crate::skill_catalog::SkillSelector, String> {
    serde_json::from_value(value.clone()).map_err(|error| {
        format!(
            "invalid run_skill_capability selector: expected exactly one nonempty name or path \
             object: {error}"
        )
    })
}

fn authorize_mcp_skill_selector(
    selector: &crate::skill_catalog::SkillSelector,
    config: &Config,
) -> Result<(), String> {
    let crate::skill_catalog::SkillSelector::Path(selector) = selector else {
        return Ok(());
    };
    let expanded =
        crate::util::expand_tilde_required(&selector.path).map_err(|error| format!("{error:#}"))?;
    let package_root = if expanded.file_name().and_then(|name| name.to_str()) == Some("SKILL.md") {
        expanded
            .parent()
            .ok_or_else(|| {
                "run_skill_capability SKILL.md path has no package directory".to_string()
            })?
            .to_path_buf()
    } else {
        expanded
    };
    let canonical = package_root.canonicalize().map_err(|error| {
        format!(
            "failed to resolve run_skill_capability path {}: {error}",
            package_root.display()
        )
    })?;
    let authorized = config.skills.search_paths.iter().any(|configured| {
        crate::util::expand_tilde(configured)
            .canonicalize()
            .is_ok_and(|root| canonical == root || canonical.starts_with(&root))
    });
    if !authorized {
        return Err(format!(
            "run_skill_capability path {} is outside configured [skills].search_paths; add its \
             parent there or select a discovered skill by name",
            canonical.display()
        ));
    }
    Ok(())
}

fn tool_run_skill_capability(
    args: &Value,
    config: &Config,
    db: &Db,
) -> Result<ToolResponse, String> {
    let all_results = mcp_bool_arg(args, "all_results", false);
    let requested_limit = mcp_optional_positive_usize_arg(args, "limit")?;
    if all_results && requested_limit.is_some() {
        return Err(
            "run_skill_capability accepts limit or all_results, not both: limit asks for one \
             page and all_results asks for every match. Drop one."
                .to_string(),
        );
    }
    // `0` means unlimited to the core query, which is exactly what all_results asks for. The MCP
    // surface never spells it that way, because a page size of zero reads like "no results".
    let page_limit = if all_results {
        0
    } else {
        requested_limit.unwrap_or(config.mcp.run_message_classification_limit)
    };
    let query_limit = if page_limit == 0 {
        0
    } else {
        page_limit.checked_add(1).ok_or_else(|| {
            "run_skill_capability limit is too large to compute pagination".to_string()
        })?
    };
    let offset = mcp_nonnegative_usize_arg(args, "offset", 0)?;

    let (since, until) = parse_date_bounds(args, chrono::Utc::now())?;
    let session_id = match args.get("session_id").and_then(Value::as_str) {
        Some(id) => Some(
            db.resolve_session_record(id)
                .map(|session| session.id)
                .map_err(|error| format!("{error:#}"))?,
        ),
        None => None,
    };
    let filters = MessageFilters {
        providers: parse_opt_enum::<Provider>(args, "provider")?.map(|provider| vec![provider]),
        session_id,
        path_prefix: args
            .get("workspace_path_prefix")
            .and_then(Value::as_str)
            .map(normalize_path_prefix),
        since,
        until,
        limit: query_limit,
        offset,
        // Left as `None` when the caller named no class, so the core applies its own user-only
        // default. Deciding it here would put "what a correction IS" in the MCP adapter, where
        // the CLI and Python could not inherit it.
        session_kinds: parse_enum_array(args, "session_kinds")?,
        ..Default::default()
    };
    filters
        .validate("run_skill_capability")
        .map_err(|error| format!("{error:#}"))?;

    let primary = parse_mcp_skill_selector(
        args.get("skill")
            .ok_or_else(|| "run_skill_capability requires skill".to_string())?,
    )?;
    let additional = args
        .get("additional_skills")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .map(parse_mcp_skill_selector)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();
    for selector in std::iter::once(&primary).chain(additional.iter()) {
        authorize_mcp_skill_selector(selector, config)?;
    }
    let definition = args
        .get("definition")
        .cloned()
        .map(serde_json::from_value::<crate::skill_run::MessageClassificationDefinition>)
        .transpose()
        .map_err(|error| {
            format!(
                "invalid run_skill_capability definition: expected categories with nonempty name \
                 and patterns fields: {error}"
            )
        })?;
    let detail = parse_message_search_detail(args)?;
    if detail.is_some() && (args.get("field_view").is_some() || args.get("match_view").is_some()) {
        return Err(
            "run_skill_capability detail conflicts with field_view and match_view; use the preset \
             or the explicit budgets, not both"
                .to_string(),
        );
    }
    let field_budget = match detail {
        Some(DetailLevel::Full) => FieldViewBudget::NoCharLimit,
        Some(DetailLevel::Compact) | None => {
            parse_field_view_budget(args)?.unwrap_or(FieldViewBudget::MaxChars {
                max_chars: NonZeroUsize::new(config.mcp.preview_chars.max(1))
                    .expect("max(1) is nonzero"),
            })
        }
    };
    let match_budget = parse_match_view_budget(args)?.unwrap_or(MatchViewBudget::MaxChars {
        max_chars: NonZeroUsize::new(DEFAULT_MATCH_EVIDENCE_MAX_CHARS)
            .expect("the match-view default is nonzero"),
    });
    let mut run = crate::service::AnalysisService::new(config, db)
        .run_skill(&crate::skill_run::SkillRunQuery {
            skill: primary,
            definition,
            input: crate::skill_run::SkillCapabilityInput::MessageClassification(
                crate::skill_run::MessageClassificationQuery {
                    filters,
                    additional_skills: additional,
                },
            ),
        })
        .map_err(|error| format!("{error:#}"))?;
    let crate::skill_run::SkillCapabilityOutput::MessageClassification(output) = &mut run.output;
    let has_more = page_limit > 0 && output.report.matches.len() > page_limit;
    if has_more {
        output.report.matches.truncate(page_limit);
    }
    let returned = output.report.matches.len();
    let presented_matches = output
        .report
        .matches
        .iter()
        .map(|matched| {
            let (field_view, match_view) = crate::message_search::classification_presentation(
                &matched.content,
                matched.match_start_char,
                matched.match_end_char_exclusive,
                field_budget,
                match_budget,
            )
            .map_err(|error| format!("{error:#}"))?;
            Ok(json!({
                "message_ref": {
                    "session_id": matched.session_id,
                    "message_seq": matched.message_seq
                },
                "message_metadata": {
                    "provider": matched.provider,
                    "timestamp": matched.ts
                },
                "classification": {
                    "policy_name": matched.policy_name,
                    "category": matched.category,
                    "matched_text": matched.matched_text,
                    "field_start_char": matched.match_start_char,
                    "field_end_char_exclusive": matched.match_end_char_exclusive,
                    "coordinate_unit": "unicode_scalar"
                },
                "presentation": {
                    "field_view": field_view,
                    "match_view": match_view
                }
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let next_offset = if has_more {
        Some(offset.checked_add(returned).ok_or_else(|| {
            "run_skill_capability offset overflowed while computing the next page".to_string()
        })?)
    } else {
        None
    };
    let mut run_value = serde_json::to_value(&run).map_err(|error| format!("{error:#}"))?;
    run_value["output"]["result"]["report"]["matches"] = Value::Array(presented_matches);
    let value = json!({
        "run": run_value,
        "returned": returned,
        "next_offset": next_offset,
        "pagination": {
            "limit": (page_limit > 0).then_some(page_limit),
            "offset": offset,
            "ordering": "timestamp desc, session id asc, sequence asc",
        }
    });
    ToolResponse::structured(value)
}

#[cfg(test)]
fn tool_query_session_index(args: &Value, config: &Config) -> Result<ToolResponse, String> {
    tool_query_session_index_cancellable(args, config, None)
}

fn tool_query_session_index_cancellable(
    args: &Value,
    config: &Config,
    cancellation: Option<&QueryCancellation>,
) -> Result<ToolResponse, String> {
    let sql = args
        .get("sql")
        .and_then(Value::as_str)
        .filter(|sql| !sql.trim().is_empty());
    let schema_table = args.get("schema_table").and_then(Value::as_str);
    if sql.is_some() && schema_table.is_some() {
        return Err(
            "query_session_index accepts one mode at a time: provide sql to run a read-only query over the AI session-history index, schema_table to inspect columns, or neither to list schema objects.".to_string(),
        );
    }
    if sql.is_some() {
        crate::search_scope::ensure_raw_sql_allowed(
            &config.search.scope,
            "query_session_index SQL",
        )
        .map_err(|error| format!("{error:#}"))?;
    }
    if sql.is_none() {
        let schema_args = DbSchemaArgs {
            table: schema_table.map(str::to_string),
            include_internal: mcp_bool_arg(args, "include_internal", false),
            format: crate::render::OutputFormat::Json,
        };
        let result = match cancellation {
            Some(cancellation) => sql_query::schema_path_cancellable(
                &config.db_path(),
                config.index.busy_timeout_ms,
                &schema_args,
                cancellation,
            ),
            None => sql_query::schema_path(
                &config.db_path(),
                config.index.busy_timeout_ms,
                &schema_args,
            ),
        }
        .map_err(format_mcp_query_error)?;
        let payload =
            sql_query::query_result_payload(&result, 0, mcp_max_cell_chars(args, config)?);
        return ToolResponse::structured(payload.value);
    }

    let query_args = ResolvedDbQueryArgs {
        sql: sql.unwrap().to_string(),
        limit: mcp_nonnegative_usize_arg(args, "limit", config.db.query_limit)?,
        offset: mcp_nonnegative_usize_arg(args, "offset", 0)?,
        timeout_ms: mcp_u64_arg(args, "timeout_ms", config.mcp.query_timeout_ms),
        format: crate::render::OutputFormat::Json,
    };
    let result = match cancellation {
        Some(cancellation) => sql_query::query_path_cancellable(
            &config.db_path(),
            config.index.busy_timeout_ms,
            &query_args,
            cancellation,
        ),
        None => sql_query::query_path(&config.db_path(), config.index.busy_timeout_ms, &query_args),
    }
    .map_err(format_mcp_query_error)?;
    let payload = sql_query::query_result_payload(
        &result,
        query_args.offset,
        mcp_max_cell_chars(args, config)?,
    );
    ToolResponse::structured(payload.value)
}

fn format_mcp_query_error(err: anyhow::Error) -> String {
    sql_query::format_query_error(
        err,
        "query_session_index",
        "call query_session_index with no sql to list AI session-history tables, or schema_table to inspect columns",
    )
}

fn mcp_max_cell_chars(args: &Value, config: &Config) -> Result<usize, String> {
    mcp_nonnegative_usize_arg(args, "max_cell_chars", config.mcp.query_max_cell_chars)
}

fn mcp_bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn mcp_u64_arg(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn mcp_nonnegative_usize_arg(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    let Some(value) = args.get(key) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| format!("{key} must be a non-negative integer"))?;
    let maximum = max_mcp_numeric_usize();
    let value = usize::try_from(value)
        .map_err(|_| format!("{key} must be 0 through {maximum}, got {value}"))?;
    if value > maximum {
        return Err(format!("{key} must be 0 through {maximum}, got {value}"));
    }
    Ok(value)
}

fn max_mcp_numeric_usize() -> usize {
    usize::try_from(i64::MAX).unwrap_or(usize::MAX)
}

fn mcp_positive_usize_arg(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    let value = mcp_nonnegative_usize_arg(args, key, default)?;
    if value == 0 {
        return Err(format!(
            "{key} must be 1 through {}, got 0",
            max_mcp_numeric_usize()
        ));
    }
    Ok(value)
}

/// `preview_chars` where omitting it means "complete text" rather than falling back to a
/// configured default. Returns `None` when absent, so a caller who never sets it is never
/// silently truncated; a supplied value is validated exactly like `mcp_positive_usize_arg`.
fn mcp_optional_positive_usize_arg(args: &Value, key: &str) -> Result<Option<usize>, String> {
    if args.get(key).is_none_or(Value::is_null) {
        return Ok(None);
    }
    mcp_positive_usize_arg(args, key, 1).map(Some)
}

fn inspection_options_from_args(
    args: &Value,
    config: &Config,
) -> Result<InspectionOptions, String> {
    Ok(InspectionOptions {
        preview_chars: mcp_positive_usize_arg(
            args,
            "preview_chars",
            config.mcp.preview_chars.max(1),
        )?,
        evidence_window: crate::inspect::EvidenceWindow::from_signed_items(
            args.get("summary_items")
                .and_then(Value::as_i64)
                .unwrap_or(config.mcp.summary_items),
        )
        .map_err(|error| format!("{error:#}"))?,
        include_time_profile: false,
    })
}

fn reject_non_default(
    args: &Value,
    key: &str,
    default: Value,
    message: &str,
) -> Result<(), String> {
    if args
        .get(key)
        .is_some_and(|value| !value.is_null() && value != &default)
    {
        Err(message.to_string())
    } else {
        Ok(())
    }
}

fn mcp_nonnegative_i64_arg(args: &Value, key: &str, default: i64) -> Result<i64, String> {
    let Some(raw) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    let value = raw
        .as_i64()
        .ok_or_else(|| format!("{key} must be a non-negative integer"))?;
    if value < 0 {
        return Err(format!("{key} must be non-negative; got {value}"));
    }
    Ok(value)
}

/// Levenshtein edit distance, used only to name the likeliest intended parameter in an
/// unknown-parameter error. Operates on `char`s so a multibyte key is never split mid-codepoint.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0usize; right.len() + 1];
    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, &right_char) in right.iter().enumerate() {
            let substitution = previous[j] + usize::from(left_char != right_char);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

/// Recovery hint for an unknown parameter name: name the single likeliest intended parameter when
/// one is close enough to be a typo, otherwise list every accepted parameter. Either way the
/// caller can correct the call from the error text without re-reading the schema.
///
/// The distance threshold scales with the key's length so short names ("role") do not match an
/// unrelated short name, while a longer key tolerates the extra transposition a longer word invites.
/// Nearest candidate to `name` within a length-scaled edit distance, or `None` when nothing is
/// close enough to suggest. Shared by the unknown-parameter and unknown-tool messages so both
/// name errors use one threshold and cannot drift apart.
///
/// The threshold scales with the name's length so short names ("role") do not match an unrelated
/// short name, while a longer name tolerates the extra transposition a longer word invites.
fn nearest_name<'a>(name: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let threshold = (name.chars().count() / 3).clamp(1, 3);
    candidates
        .iter()
        .map(|candidate| (edit_distance(name, candidate), *candidate))
        .filter(|(distance, _)| *distance <= threshold)
        .min_by_key(|(distance, candidate)| (*distance, candidate.len()))
        .map(|(_, candidate)| candidate)
}

fn unknown_key_hint(key: &str, accepted: &[&str]) -> String {
    if accepted.is_empty() {
        return String::new();
    }
    match nearest_name(key, accepted) {
        Some(candidate) => format!(" — did you mean {candidate:?}?"),
        None => {
            let mut accepted: Vec<&str> = accepted.to_vec();
            accepted.sort_unstable();
            let accepted: Vec<String> = accepted.iter().map(|name| format!("{name:?}")).collect();
            format!(" — accepted parameters are {}", accepted.join(", "))
        }
    }
}

/// Quoted, comma-separated list of every tool name in the served tool list, in declaration order,
/// for inclusion in an unknown-tool error. Mirrors the `must be one of "a", "b"` phrasing used for
/// invalid enum arguments so both classes of name error read the same way.
fn known_tool_names(tools: &Value) -> String {
    tool_name_list(tools)
        .iter()
        .map(|name| format!("{name:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Served tool names in declaration order, for both the catalogue text and the nearest-match hint.
fn tool_name_list(tools: &Value) -> Vec<&str> {
    tools
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect()
        })
        .unwrap_or_default()
}

/// Error text for a tool name this server does not serve. Leads with the likeliest intended tool
/// when one is close, then always lists the catalogue so a caller whose guess was wrong still
/// recovers from this one message without a second `tools/list` call.
fn unknown_tool_message(tool_name: &str, tools: &Value) -> String {
    let names = tool_name_list(tools);
    let catalogue = known_tool_names(tools);
    if catalogue.is_empty() {
        return format!("unknown tool: {tool_name} — this server provides no tools");
    }
    match nearest_name(tool_name, &names) {
        Some(candidate) => format!(
            "unknown tool: {tool_name} — did you mean {candidate:?}? this server provides {catalogue}"
        ),
        None => format!("unknown tool: {tool_name} — this server provides {catalogue}"),
    }
}

/// Signed line-count argument (`lines_per_message`): positive=head, negative=tail, 0=unlimited.
fn mcp_i64_arg(args: &Value, key: &str, default: i64) -> i64 {
    args.get(key).and_then(Value::as_i64).unwrap_or(default)
}

/// Parse an optional enum argument (e.g. `role`, `provider`) via its `FromStr`. Absent →
/// `None`; present-but-invalid → a clear error string surfaced to the agent.
fn parse_opt_enum<T>(args: &Value, key: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr<Err = String>,
{
    args.get(key)
        .and_then(Value::as_str)
        .map(str::parse::<T>)
        .transpose()
        .map_err(|e| e.to_string())
}

/// `include` member that restores `raw_metadata_json` on session records.
///
/// The field is the provider's metadata blob copied verbatim. For codex it embeds the whole
/// sandbox policy as escaped JSON, ~2-3 KB per session; measured over a 30-session listing it
/// was 24,929 of 56,667 characters (44%), and it is why `list_sessions(limit=30)` failed with
/// "result (55,824 characters) exceeds maximum allowed tokens". Session-level tools have no
/// field selection, so a caller had no way to ask for less. It is therefore omitted by default
/// and restored on request, reusing the same `include` mechanism `get_session` already uses for
/// `time_profile` rather than introducing a second way to say the same thing.
const INCLUDE_RAW_METADATA: &str = "raw_metadata";

/// Remove `raw_metadata_json` from every session record in a serialized payload unless the
/// caller asked for it. Operates on the serialized value so all three session-returning tools
/// share one rule and cannot drift apart.
fn apply_raw_metadata_include(value: &mut Value, include: &[String]) {
    if include.iter().any(|item| item == INCLUDE_RAW_METADATA) {
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                apply_raw_metadata_include(item, include);
            }
        }
        Value::Object(map) => {
            map.remove("raw_metadata_json");
            for (_, nested) in map.iter_mut() {
                apply_raw_metadata_include(nested, include);
            }
        }
        _ => {}
    }
}

/// Every `MessageKind` spelling, derived from the enum rather than written out, so a new
/// member reaches the MCP schema automatically. Two hand-maintained copies of this list had
/// already been written, which is exactly how a schema drifts from the values it accepts.
fn message_kind_values() -> Vec<&'static str> {
    use clap::ValueEnum;
    crate::models::MessageKind::value_variants()
        .iter()
        .map(|kind| kind.as_str())
        .collect()
}

/// Every `SessionKind` spelling, derived from the enum for the same reason as
/// [`message_kind_values`]: a hand-written copy of an accepted-values list drifts from the
/// values the parser accepts, and the drift is invisible until a caller is rejected.
fn session_kind_values() -> Vec<&'static str> {
    crate::models::SessionKind::all()
        .into_iter()
        .map(crate::models::SessionKind::as_str)
        .collect()
}

/// Which classes of session a session tool returns. Shared by `search_sessions` and
/// `list_sessions` so the two cannot describe the same parameter differently.
fn session_kinds_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": session_kind_values() },
        "description": "Which classes of session to return. 'user' is a session a person started; 'subagent' is a run one of those spawned, with parent_session_id naming the spawner and agent_label naming the kind of agent (Explore, general-purpose, a codex agent nickname). Omit for both. Pass ['subagent'] to search only delegated work, or ['user'] to list conversations without the runs beneath them. An empty array matches nothing."
    })
}

/// The spawned-by selector, shared by the session tools for the same reason.
fn parent_session_id_schema() -> Value {
    json!({
        "type": "string",
        "description": "Only runs spawned by this exact session, given as that session's id (the `id` field, e.g. 'claude:7e745098-...'). Answers 'what did this session delegate'. Omit to match sessions with any parent or none."
    })
}

fn purpose_input_schema(config: &Config) -> Value {
    let names = config.search.purposes.keys().cloned().collect::<Vec<_>>();
    if names.is_empty() {
        json!({
            "type": "string",
            "description": "None configured; omit. Add [search.purposes.<name>] to define one."
        })
    } else {
        json!({
            "type": "string",
            "enum": names,
            "description": format!(
                "Select one configured message-search purpose name. Available in this server: {}.",
                config.search.purposes.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }
}

/// The one parameter the MCP adapter injects into every tool rather than any tool declaring it.
///
/// It is not one of the planner's canonical parameters, so the registry does not hold it and no
/// canonical projection names it. Both directions of the advertised-equals-owned check have to
/// account for it by name.
const ADAPTER_CONTROL_FIELD: &str = "index_refresh";

/// The one `search_messages` field whose description a more specific generator already owns.
///
/// `purpose_input_schema` renders the bundles this deployment configured. The registry describes
/// what the parameter selects and cannot know which bundles exist, so the generic rendering would
/// replace a list of real choices with a sentence about choices in general.
const GENERATED_ELSEWHERE_FIELD: &str = "purpose";

fn add_index_refresh_controls(response: &mut Value) {
    let Some(tools) = response
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for tool in tools {
        let Some(properties) = tool
            .get_mut("inputSchema")
            .and_then(|schema| schema.get_mut("properties"))
            .and_then(Value::as_object_mut)
        else {
            continue;
        };
        properties.insert(
            ADAPTER_CONTROL_FIELD.to_string(),
            // `x-aise-adapter-control: true` used to mark this as injected by the adapter rather
            // than declared by the tool. It rode on all eight tools and no client reads it; the
            // fact it carried is asserted directly against this function by
            // `index_refresh_is_injected_into_every_advertised_tool`.
            json!({
                "type": "string",
                "enum": ["auto", "existing-only"],
                "default": "auto",
                // The single highest-leverage description in the payload: one edit here lands on
                // all eight tools. It also names the write, which the readOnlyHint annotation
                // does not: `auto` may index new transcripts, so a tool declared read-only can
                // write on its default setting. A reader should not have to infer that from an
                // annotation the specification tells clients to treat as untrusted.
                "description": "Index-read policy. Omit for auto, which may index new transcripts; existing-only never writes."
            }),
        );
    }
}

/// Add one shared non-executed result alternative without duplicating each tool's normal schema.
///
/// Schema construction is `O(T)` in the fixed advertised-tool count. Normal result objects keep
/// their established shape; only the unavailable-snapshot alternative carries readiness and the
/// exact status call. This avoids permanent metadata overhead on every successful search.
fn add_not_ready_output_alternatives(response: &mut Value) {
    let Some(tools) = response
        .get_mut("result")
        .and_then(|result| result.get_mut("tools"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    for tool in tools {
        if tool.get("name").and_then(Value::as_str) == Some("get_index_status") {
            continue;
        }
        let Some(schema) = tool.get_mut("outputSchema").and_then(Value::as_object_mut) else {
            continue;
        };
        let not_ready_schema = json!({
            "type": "object",
            "properties": {
                "operation_executed": {
                    "const": false,
                    "description": "The requested tool operation was not executed because no usable index snapshot exists."
                },
                "index_status": index_readiness_output_schema(),
                "next_call": {
                    "type": "object",
                    "properties": {
                        "tool": { "const": "get_index_status" },
                        "arguments": { "type": "object", "maxProperties": 0 }
                    },
                    "required": ["tool", "arguments"],
                    "additionalProperties": false
                }
            },
            "required": ["operation_executed", "index_status", "next_call"],
            "additionalProperties": false
        });
        if let Some(alternatives) = schema.get_mut("oneOf").and_then(Value::as_array_mut) {
            alternatives.push(not_ready_schema);
            continue;
        }
        let normal_required = schema
            .remove("required")
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
            continue;
        };
        properties.insert(
            "operation_executed".to_string(),
            json!({
                "const": false,
                "description": "The requested tool operation was not executed because no usable index snapshot exists."
            }),
        );
        properties.insert("index_status".to_string(), index_readiness_output_schema());
        properties.insert(
            "next_call".to_string(),
            json!({
                "type": "object",
                "properties": {
                    "tool": { "const": "get_index_status" },
                    "arguments": { "type": "object", "maxProperties": 0 }
                },
                "required": ["tool", "arguments"],
                "additionalProperties": false
            }),
        );
        schema.insert(
            "oneOf".to_string(),
            json!([
                { "required": normal_required },
                {
                    "properties": { "operation_executed": { "const": false } },
                    "required": ["operation_executed", "index_status", "next_call"]
                }
            ]),
        );
    }
}

/// Free-text fields on a session record and a search hit. Everything else on the record is an
/// id, a path, a timestamp, or a count, none of which is safe or useful to truncate.
const SESSION_PREVIEW_FIELDS: [&str; 4] = ["title", "summary", "preview_text", "match_snippet"];

/// Bound each free-text field on every session record to `preview_chars` characters.
///
/// `None` leaves the text complete, which is the default so no existing caller is silently
/// truncated. This is the session-level counterpart to `search_messages`'s `preview_chars`:
/// before it, the session tools had no payload control at all, which is what left a caller
/// facing the MCP token cap with nothing to turn down.
fn apply_session_preview_chars(value: &mut Value, preview_chars: Option<usize>) {
    let Some(limit) = preview_chars else {
        return;
    };
    match value {
        Value::Array(items) => {
            for item in items {
                apply_session_preview_chars(item, preview_chars);
            }
        }
        Value::Object(map) => {
            for field in SESSION_PREVIEW_FIELDS {
                if let Some(Value::String(text)) = map.get_mut(field) {
                    *text = truncate_for_display(text, limit);
                }
            }
            for (_, nested) in map.iter_mut() {
                apply_session_preview_chars(nested, preview_chars);
            }
        }
        _ => {}
    }
}

/// Schema for `preview_chars` on the session-returning tools.
fn session_preview_chars_schema() -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
                "description": format!(
            "Maximum characters for each of this record's free-text fields ({}). Accepts a positive count; omit it to return the complete text, which is the default. Use it to keep a large page within the response limit; it changes presentation only, never which sessions match, their order, or the result count.",
            SESSION_PREVIEW_FIELDS.join(", ")
        )
    })
}

/// Schema for the `include` array on the session-returning tools.
fn raw_metadata_include_schema() -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": [INCLUDE_RAW_METADATA] },
        "description": "Optional extra fields (default none). 'raw_metadata' restores each record's raw_metadata_json, the provider's verbatim metadata blob, which is omitted by default because it is unbounded: codex embeds its entire sandbox policy, about 2-3 KB per session.",
        "default": []
    })
}

fn parse_string_array(args: &Value, key: &str) -> Result<Vec<String>, String> {
    let Some(value) = args.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array of strings"))?;
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{key}[{i}] must be a string"))
        })
        .collect()
}

/// Parse an optional date argument with the shared `dates` grammar (EDTF / ISO / duration /
/// natural language), resolving to the requested `bound` of its period. Reuses the exact
/// parser the CLI `--since/--until` flags use, so MCP and CLI accept identical date strings.
fn parse_date_arg(
    args: &Value,
    key: &str,
    bound: Bound,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<chrono::DateTime<chrono::Utc>>, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(|raw| dates::parse_bound(raw, bound, now).map_err(|e| format!("invalid {key}: {e}")))
        .transpose()
}

fn parse_date_bounds(
    args: &Value,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<dates::Bounds, String> {
    if let Some(raw) = args.get("when").and_then(Value::as_str) {
        if args.get("since").and_then(Value::as_str).is_some()
            || args.get("until").and_then(Value::as_str).is_some()
        {
            return Err("provide `when` OR `since`/`until`, not both".to_string());
        }
        let (since, until) =
            dates::parse_span(raw, now).map_err(|e| format!("invalid when: {e}"))?;
        return Ok((Some(since), Some(until)));
    }
    Ok((
        parse_date_arg(args, "since", Bound::Start, now)?,
        parse_date_arg(args, "until", Bound::End, now)?,
    ))
}

fn search_filters_from_args(
    args: &Value,
    default_limit: usize,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<SearchFilters, String> {
    let (since, until) = parse_date_bounds(args, now)?;
    Ok(SearchFilters {
        provider: parse_opt_enum::<Provider>(args, "provider")?,
        path_prefix: args
            .get("path_prefix")
            .and_then(Value::as_str)
            .map(normalize_path_prefix),
        exclude_path_prefixes: parse_string_array(args, "exclude_path_prefixes")?
            .into_iter()
            .map(|path| normalize_path_prefix(&path))
            .collect(),
        exclude_session_ids: parse_string_array(args, "exclude_session_ids")?,
        // `session_kinds` is the single session-class selector. Do not add per-class booleans
        // beside it: one was tried for message classes and reverted because it duplicated the
        // set and self-cancelled against it. See models.rs SearchFilters::session_kinds.
        session_kinds: parse_enum_array(args, "session_kinds")?,
        parent_session_id: args
            .get("parent_session_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        since,
        until,
        limit: mcp_nonnegative_usize_arg(args, "limit", default_limit)?,
        warnings_only: false,
    })
}

/// Read an optional array-of-enum-names argument.
///
/// Absent yields `None` so the caller's own default set applies — distinct from an empty array,
/// which is a caller who deselected every class and gets no rows rather than all of them. A
/// non-array or an unparseable name is an error carrying the accepted values, which is the
/// enum's own `FromStr` message, so the schema and the rejection cannot disagree.
fn parse_enum_array<T>(args: &Value, key: &str) -> Result<Option<Vec<T>>, String>
where
    T: std::str::FromStr<Err = String>,
{
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let entries = value
        .as_array()
        .ok_or_else(|| format!("{key} must be an array of strings"))?;
    let mut parsed = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .as_str()
            .ok_or_else(|| format!("{key} entries must be strings"))?;
        parsed.push(name.parse::<T>()?);
    }
    Ok(Some(parsed))
}

fn parse_message_search_detail(args: &Value) -> Result<Option<DetailLevel>, String> {
    let Some(value) = args.get("detail") else {
        return Ok(None);
    };
    match value.as_str() {
        Some("compact") => Ok(Some(DetailLevel::Compact)),
        Some("full") => Ok(Some(DetailLevel::Full)),
        Some(other) => Err(format!("detail must be compact or full; got {other:?}")),
        None => Err("detail must be a string: compact or full".to_string()),
    }
}

fn parse_field_view_budget(args: &Value) -> Result<Option<FieldViewBudget>, String> {
    args.get("field_view")
        .map(|value| {
            crate::message_search::decode_field_view_budget(value, max_mcp_numeric_usize())
                .map_err(|error| error.to_string())
        })
        .transpose()
}

fn parse_match_view_budget(args: &Value) -> Result<Option<MatchViewBudget>, String> {
    args.get("match_view")
        .map(|value| {
            crate::message_search::decode_match_view_budget(value, max_mcp_numeric_usize())
                .map_err(|error| error.to_string())
        })
        .transpose()
}

fn parse_message_search_includes(
    args: &Value,
) -> Result<Option<Vec<MessageSearchInclude>>, String> {
    let Some(value) = args.get("include") else {
        return Ok(None);
    };
    let entries = value
        .as_array()
        .ok_or_else(|| "include must be an array of strings".to_string())?;
    let mut includes = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = entry
            .as_str()
            .ok_or_else(|| "include entries must be strings".to_string())?;
        includes.push(match name {
            "normalized_session_metadata" => MessageSearchInclude::NormalizedSessionMetadata,
            "parsed_references" => MessageSearchInclude::ParsedReferences,
            "raw_provider_metadata" => MessageSearchInclude::RawProviderMetadata,
            "runtime_diagnostics" => MessageSearchInclude::RuntimeDiagnostics,
            other => {
                return Err(format!(
                    "unknown include {other:?}; accepted values are normalized_session_metadata, parsed_references, raw_provider_metadata, and runtime_diagnostics"
                ))
            }
        });
    }
    Ok(Some(includes))
}

#[cfg(test)]
fn tool_search_messages(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    tool_search_messages_cancellable(args, config, db, None)
}

fn tool_search_messages_cancellable(
    args: &Value,
    config: &Config,
    db: &Db,
    cancellation: Option<&AtomicBool>,
) -> Result<ToolResponse, String> {
    if args.get("kind").is_some() && args.get("kinds").is_some() {
        return Err(
            "kind and kinds cannot be used together; use kind for one class or kinds for several"
                .to_string(),
        );
    }
    let query_text = args.get("query").and_then(Value::as_str).unwrap_or("");
    let query_mode = args
        .get("query_mode")
        .and_then(Value::as_str)
        .unwrap_or("literal");
    let query = match (query_mode, query_text.is_empty()) {
        ("literal", true) => Ok(MessageQuery::All),
        ("literal", false) => MessageQuery::literal(query_text),
        ("regex", false) => MessageQuery::regex(query_text),
        ("fuzzy", false) => MessageQuery::fuzzy(query_text),
        ("regex" | "fuzzy", true) => {
            return Err(format!("query_mode={query_mode} requires a nonempty query"))
        }
        (other, _) => {
            return Err(format!(
                "query_mode must be literal, regex, or fuzzy; got {other:?}"
            ))
        }
    }
    .map_err(|error| error.to_string())?;
    let field = parse_opt_enum::<crate::models::SearchField>(args, "field")?
        .unwrap_or(crate::models::SearchField::Content);
    let target = match field {
        crate::models::SearchField::Content => MessageTarget::content(),
        crate::models::SearchField::ToolName => MessageTarget::tool_name(),
        crate::models::SearchField::ToolArgument => MessageTarget::tool_argument(
            args.get("argument_path")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )
        .map_err(|error| error.to_string())?,
    };
    let offset = mcp_nonnegative_usize_arg(args, "offset", 0)?;
    if args.get("session").is_some() {
        return Err(
            "unknown parameter `session`; use `session_id` with an exact ID or unique prefix"
                .to_string(),
        );
    }
    let (since, until) = parse_date_bounds(args, chrono::Utc::now())?;
    let mut builder = MessageSearchRequest::builder(query, target)
        .time(RequestedTimeRange::new(since, until).map_err(|error| error.to_string())?)
        .include_compaction(mcp_bool_arg(args, "include_compaction", true));
    // `kinds` is the single class-selection mechanism; `kind` is its one-value alias. Do not
    // reintroduce per-class booleans here: one was tried and reverted because it duplicated
    // this parameter and self-cancelled against it. See models.rs MessageFilters::kinds.
    if let Some(kinds) = parse_enum_array::<crate::models::MessageKind>(args, "kinds")? {
        builder = builder.kinds(kinds);
    }
    let all_results = mcp_bool_arg(args, "all_results", false);
    if all_results && args.get("limit").is_some() {
        return Err("all_results conflicts with limit".to_string());
    }
    let requested_limit = args
        .get("limit")
        .map(|_| mcp_nonnegative_usize_arg(args, "limit", 0))
        .transpose()?;
    if requested_limit == Some(0) {
        return Err(
            "limit must be greater than zero; use all_results=true for every match".to_string(),
        );
    }
    builder = builder.extent(if all_results {
        RequestedExtent::all_results_from(offset)
    } else {
        RequestedExtent::page(requested_limit, offset).map_err(|error| error.to_string())?
    });
    if let Some(role) = parse_opt_enum::<Role>(args, "role")? {
        builder = builder.role(role);
    }
    if let Some(kind) = parse_opt_enum::<crate::models::MessageKind>(args, "kind")? {
        builder = builder.kind(kind);
    }
    if let Some(providers) = parse_enum_array::<Provider>(args, "providers")? {
        builder = builder
            .providers(providers)
            .map_err(|error| error.to_string())?;
    }
    if let Some(session) = args.get("session_id").and_then(Value::as_str) {
        builder = builder
            .session_id(session)
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = args.get("workspace_path_prefix").and_then(Value::as_str) {
        builder = builder
            .workspace_path_prefix(path)
            .map_err(|error| error.to_string())?;
    }
    if let Some(path) = args.get("transcript_path_prefix").and_then(Value::as_str) {
        builder = builder
            .transcript_path_prefix(path)
            .map_err(|error| error.to_string())?;
    }
    for path in parse_string_array(args, "exclude_workspace_path_prefixes")? {
        builder = builder
            .exclude_workspace_path_prefix(path)
            .map_err(|error| error.to_string())?;
    }
    for path in parse_string_array(args, "exclude_transcript_path_prefixes")? {
        builder = builder
            .exclude_transcript_path_prefix(path)
            .map_err(|error| error.to_string())?;
    }
    for session in parse_string_array(args, "exclude_session_ids")? {
        builder = builder
            .exclude_session_id(session)
            .map_err(|error| error.to_string())?;
    }
    let seq_from = args.get("seq_from").and_then(Value::as_i64);
    let seq_to = args.get("seq_to").and_then(Value::as_i64);
    if seq_from.is_some() || seq_to.is_some() {
        builder = builder
            .sequence(SequenceRange::new(seq_from, seq_to).map_err(|error| error.to_string())?);
    }
    if let Some(tool) = args.get("tool_name_contains").and_then(Value::as_str) {
        builder = builder
            .tool_name_contains(tool)
            .map_err(|error| error.to_string())?;
    }
    if let Some(window) = args.get("match_window").and_then(Value::as_str) {
        builder = builder.match_window(match window {
            "earliest" => MatchWindow::Earliest,
            "latest" => MatchWindow::Latest,
            other => {
                return Err(format!(
                    "match_window must be earliest or latest; got {other:?}"
                ))
            }
        });
    }
    let symmetric = mcp_nonnegative_i64_arg(args, "context", 0)?;
    if args.get("context").is_some()
        || args.get("context_before").is_some()
        || args.get("context_after").is_some()
    {
        let before = mcp_nonnegative_i64_arg(args, "context_before", symmetric)?;
        let after = mcp_nonnegative_i64_arg(args, "context_after", symmetric)?;
        builder = builder.context(ContextWindow::new(
            usize::try_from(before).map_err(|_| "context_before exceeds usize".to_string())?,
            usize::try_from(after).map_err(|_| "context_after exceeds usize".to_string())?,
        ));
    }
    if let Some(detail) = parse_message_search_detail(args)? {
        builder = builder.detail(detail);
    }
    if let Some(value) = args.get("lines_per_message") {
        let lines = value
            .as_i64()
            .ok_or_else(|| "lines_per_message must be an integer".to_string())?;
        builder = builder
            .message_lines(LineWindow::from_signed(lines).map_err(|error| error.to_string())?);
    }
    if let Some(budget) = parse_field_view_budget(args)? {
        builder = builder.field_view(budget);
    }
    if let Some(budget) = parse_match_view_budget(args)? {
        builder = builder.match_view(budget);
    }
    if let Some(includes) = parse_message_search_includes(args)? {
        builder = builder.includes(includes);
    }
    let purpose = args.get("purpose").and_then(Value::as_str);
    if purpose.is_none() && args.get("purpose_version").is_some() {
        return Err("purpose_version requires purpose".to_string());
    }
    if let Some(purpose) = purpose {
        let version = args
            .get("purpose_version")
            .map(|value| {
                let value = value
                    .as_u64()
                    .ok_or_else(|| "purpose_version must be a positive integer".to_string())?;
                let value =
                    u32::try_from(value).map_err(|_| "purpose_version exceeds u32".to_string())?;
                std::num::NonZeroU32::new(value)
                    .ok_or_else(|| "purpose_version must be greater than zero".to_string())
            })
            .transpose()?;
        builder = builder
            .purpose(PurposeSelection::new(purpose, version).map_err(|error| error.to_string())?);
    }
    if let Some(receipt) = args.get("receipt_level").and_then(Value::as_str) {
        builder = builder.receipt_level(match receipt {
            "none" => ReceiptLevel::None,
            "summary" => ReceiptLevel::Summary,
            "full" => ReceiptLevel::Full,
            other => {
                return Err(format!(
                    "receipt_level must be none, summary, or full; got {other:?}"
                ))
            }
        });
    }
    let messages = MessageService::new(config, db, crate::message_search::SearchSurface::Mcp);
    let response = messages
        .search_cancellable(
            builder.build().map_err(|error| error.to_string())?,
            cancellation,
        )
        .map_err(|error| format!("{error:#}"))?;
    let text = message_search_text_summary(&response, cancellation)?;
    let structured = match cancellation {
        Some(cancellation) => serde_json::to_value(response.document_cancellable(cancellation)),
        None => serde_json::to_value(response.document()),
    }
    .map_err(|error| format!("{error:#}"))?;
    Ok(ToolResponse::structured_with_text(text, structured))
}

fn message_search_text_summary(
    response: &MessageSearchResponse,
    cancellation: Option<&AtomicBool>,
) -> Result<String, String> {
    use std::fmt::Write as _;

    ensure_message_search_response_active(cancellation)?;
    let page = response.page();
    let returned = page.returned();
    let noun = if returned == 1 { "result" } else { "results" };
    let offset = match page.extent() {
        crate::message_search::ResolvedExtent::Page { offset, .. }
        | crate::message_search::ResolvedExtent::AllResults { offset } => offset,
    };
    let continuation = page.next_offset().map_or_else(
        || "no more results".to_string(),
        |next_offset| format!("more results: search_messages(offset={next_offset})"),
    );
    let mut summary = format!(
        "{returned} {noun} at offset {offset}; {continuation}. structuredContent is the authoritative response.\n"
    );
    for (index, result) in response
        .results()
        .iter()
        .take(MESSAGE_SEARCH_TEXT_RESULT_LIMIT)
        .enumerate()
    {
        ensure_message_search_response_active(cancellation)?;
        let message_ref = result.message_ref();
        let target = response.request().target();
        let field_text =
            truncate_for_display(result.field_view().text(), MESSAGE_SEARCH_TEXT_FIELD_CHARS);
        let match_summary = result.literal_match().map_or_else(
            || {
                result.match_view().map_or_else(
                    || "no text query".to_string(),
                    |view| {
                        format!(
                            "match view {}[{}..{}]: {}",
                            target.field().as_str(),
                            view.field_start_char(),
                            view.field_end_char_exclusive(),
                            truncate_for_display(view.text(), MESSAGE_SEARCH_TEXT_FIELD_CHARS),
                        )
                    },
                )
            },
            |literal| {
                format!(
                    "match {}[{}..{}]: {}",
                    target.field().as_str(),
                    literal.field_start_char,
                    literal.field_end_char_exclusive,
                    truncate_for_display(&literal.text, MESSAGE_SEARCH_TEXT_FIELD_CHARS),
                )
            },
        );
        let _ = writeln!(
            summary,
            "{}. session={} message={} provider={} role={} {match_summary}",
            index + 1,
            message_ref.session_id(),
            message_ref.message_seq(),
            result.provider.as_str(),
            result.role.as_str(),
        );
        let field_view = result.field_view();
        let _ = writeln!(
            summary,
            "   field {}[{}..{}]: {} [additional field text: {}]",
            target.field().as_str(),
            field_view.field_start_char(),
            field_view.field_end_char_exclusive(),
            field_text,
            field_view.extent().additional_field_text().as_str(),
        );
        let _ = writeln!(
            summary,
            "   full message: get_session(session_id={:?}, message_seq={}, context={})",
            message_ref.session_id(),
            message_ref.message_seq(),
            GET_SESSION_FOLLOW_UP_CONTEXT,
        );
    }
    if returned > MESSAGE_SEARCH_TEXT_RESULT_LIMIT {
        ensure_message_search_response_active(cancellation)?;
        let _ = writeln!(
            summary,
            "Text shows the first {} of {returned} returned results; structuredContent contains all {returned}.",
            MESSAGE_SEARCH_TEXT_RESULT_LIMIT,
        );
    }
    ensure_message_search_response_active(cancellation)?;
    Ok(summary)
}

fn ensure_message_search_response_active(cancellation: Option<&AtomicBool>) -> Result<(), String> {
    if cancellation.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        Err("MCP tool call was cancelled while preparing the search response".to_string())
    } else {
        Ok(())
    }
}

/// How message content is shaped for one response: full or concise preview, optional refs,
/// and the per-message line cap. Parsed once per tool call from the shared argument names.
struct MessagePresentation {
    include_refs: bool,
    field_view: FieldViewBudget,
    message_lines: LineWindow,
}

struct PresentedMessageContent {
    content: String,
    extent: MessageContentExtent,
}

impl MessagePresentation {
    fn from_args(args: &Value, config: &Config) -> Result<Self, String> {
        let preview_chars =
            mcp_positive_usize_arg(args, "preview_chars", config.mcp.preview_chars.max(1))?;
        let field_view = if args.get("response_format").and_then(Value::as_str) == Some("detailed")
        {
            FieldViewBudget::NoCharLimit
        } else {
            FieldViewBudget::max_chars(preview_chars).map_err(|error| error.to_string())?
        };
        let message_lines = LineWindow::from_signed(mcp_i64_arg(
            args,
            "lines_per_message",
            config.mcp.lines_per_message,
        ))
        .map_err(|error| error.to_string())?;
        Ok(Self {
            include_refs: mcp_bool_arg(args, "include_refs", false),
            field_view,
            message_lines,
        })
    }

    /// Apply the shared exact-substring presentation path, preserving absolute coordinates.
    ///
    /// Time is `O(V)` for head/full views bounded to `V` returned characters and `O(D)` for tail
    /// selection over `D` field characters. Retained memory is `O(V)`. References are extracted
    /// separately from authoritative content, so presentation never hides reference evidence.
    fn trim_with_extent(&self, content: &str) -> Result<PresentedMessageContent, String> {
        let view = selected_field_view(content, self.message_lines, self.field_view, None)
            .map_err(|error| error.to_string())?;
        let (content, extent) = view.into_content_and_extent();
        Ok(PresentedMessageContent { content, extent })
    }
}

fn message_window_value(
    session: &SessionRecord,
    seq: i64,
    context: i64,
    presentation: &MessagePresentation,
    db: &Db,
) -> Result<Value, String> {
    let before = context;
    let after = context;
    let rows = db
        .message_context(&session.id, seq, before, after)
        .map_err(|e| format!("{e:#}"))?;
    let include_refs = presentation.include_refs;
    let messages: Vec<Value> = rows
        .iter()
        .map(|c| {
            let presented = presentation.trim_with_extent(&c.content)?;
            let mut row = json!({
                "seq": c.seq,
                "role": c.role.as_str(),
                "kind": c.kind.as_str(),
                "provider": c.provider.as_str(),
                "ts": c.ts.map(|t| t.to_rfc3339()),
                "tool_name": c.tool_name,
                "tool_call_id": c.tool_call_id,
                "is_match": c.seq == seq,
                "content": presented.content,
                "content_extent": presented.extent,
            });
            if include_refs {
                let refs = extract_refs_from_text(&c.content, c.tool_name.as_deref());
                row["ref_summary"] = json!(ref_summary(&refs));
                row["refs"] = json!(refs);
            }
            Ok(row)
        })
        .collect::<Result<_, String>>()?;
    Ok(json!({
        "session_id": session.id,
        "anchor_seq": seq,
        "cwd": session.cwd,
        "repo": session.repo_root,
        "title": session.title,
        "session_metadata": session_record_meta_json(session, true),
        "messages": messages,
    }))
}

/// Read every message of `session` whose seq falls in the inclusive `[seq_from, seq_to]` range and
/// render them in the same focused shape as [`message_window_value`], so a caller can page a long
/// session by absolute seq range (seq_from = last seq + 1) instead of re-reading a larger
/// transcript_lines window. Either bound may be open; `anchor_seq` reports the requested lower
/// bound (0 when omitted) and `is_match` flags that first message of the range when seq_from is set.
fn message_range_value(
    session: &SessionRecord,
    seq_from: Option<i64>,
    seq_to: Option<i64>,
    presentation: &MessagePresentation,
    db: &Db,
) -> Result<Value, String> {
    let filters = MessageFilters {
        session_id: Some(session.id.clone()),
        seq_from,
        seq_to,
        ..MessageFilters::default()
    };
    let rows = db
        .read_session_messages(&filters, crate::db::MessageOrder::OldestFirst)
        .map_err(|e| format!("{e:#}"))?;
    let include_refs = presentation.include_refs;
    let messages: Vec<Value> = rows
        .iter()
        .map(|c| {
            let presented = presentation.trim_with_extent(&c.content)?;
            let mut row = json!({
                "seq": c.seq,
                "role": c.role.as_str(),
                "kind": c.kind.as_str(),
                "provider": c.provider.as_str(),
                "ts": c.ts.map(|t| t.to_rfc3339()),
                "tool_name": c.tool_name,
                "tool_call_id": c.tool_call_id,
                "is_match": seq_from == Some(c.seq),
                "content": presented.content,
                "content_extent": presented.extent,
            });
            if include_refs {
                let refs = extract_refs_from_text(&c.content, c.tool_name.as_deref());
                row["ref_summary"] = json!(ref_summary(&refs));
                row["refs"] = json!(refs);
            }
            Ok(row)
        })
        .collect::<Result<_, String>>()?;
    Ok(json!({
        "session_id": session.id,
        "anchor_seq": seq_from.unwrap_or(0),
        "cwd": session.cwd,
        "repo": session.repo_root,
        "title": session.title,
        "session_metadata": session_record_meta_json(session, true),
        "messages": messages,
    }))
}

fn session_record_meta_json(session: &SessionRecord, include_source_path: bool) -> Value {
    let mut out = serde_json::Map::new();
    insert_string(
        &mut out,
        "provider_session_id",
        Some(&session.provider_session_id),
    );
    insert_string(&mut out, "cwd", session.cwd.as_deref());
    insert_string(&mut out, "repo", session.repo_root.as_deref());
    insert_string(&mut out, "title", session.title.as_deref());
    insert_time(&mut out, "updated_at", session.updated_at);
    insert_time(&mut out, "last_message_at", session.last_message_at);
    if include_source_path {
        insert_string(&mut out, "source_path", Some(&session.source_path));
    }
    if let Some(count) = session.message_count {
        out.insert("message_count".to_string(), json!(count));
    }
    insert_string(&mut out, "parse_warning", session.parse_warning.as_deref());
    Value::Object(out)
}

fn insert_string(out: &mut serde_json::Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        out.insert(key.to_string(), json!(value));
    }
}

fn insert_time(
    out: &mut serde_json::Map<String, Value>,
    key: &str,
    value: Option<chrono::DateTime<chrono::Utc>>,
) {
    if let Some(value) = value {
        out.insert(key.to_string(), json!(value.to_rfc3339()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{Message, MessageSearchMode};
    use crate::util::minimal_record;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;

    #[cfg(windows)]
    const FIXTURE_PROJECT: &str = r"C:\Users\x\proj";
    #[cfg(windows)]
    const FIXTURE_OTHER_PROJECT: &str = r"C:\Users\x\other";
    #[cfg(not(windows))]
    const FIXTURE_PROJECT: &str = "/Users/x/proj";
    #[cfg(not(windows))]
    const FIXTURE_OTHER_PROJECT: &str = "/Users/x/other";

    /// A temp index holding one session rooted at [`FIXTURE_PROJECT`] with three messages,
    /// built entirely through the public API so these tests exercise the real persist path.
    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let mut parsed = minimal_record(Provider::Claude, Path::new("/x/s.jsonl"), String::new());
        parsed.session.id = "claude:test1".to_string();
        parsed.session.provider_session_id = "test1".to_string();
        parsed.session.cwd = Some(FIXTURE_PROJECT.to_string());
        parsed.session.repo_root = Some(FIXTURE_PROJECT.to_string());
        parsed.session.title = Some("Proj".to_string());
        parsed.session.message_count = Some(3);
        parsed.transcript_text = (0..405)
            .map(|i| format!("transcript line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mk = |seq: i64, role: Role, content: &str| Message {
            seq,
            role,
            ts: None,
            tool_name: None,
            kind: if role == Role::Compaction {
                crate::models::MessageKind::Compaction
            } else {
                crate::models::MessageKind::Conversation
            },
            tool_call_id: None,
            is_compaction: false,
            content: content.to_string(),
            provenance: Default::default(),
        };
        parsed.messages = vec![
            mk(0, Role::User, "alpha hello there"),
            mk(
                1,
                Role::Assistant,
                "beta world response https://example.com/paper.pdf",
            ),
            mk(2, Role::User, "gamma hello again"),
        ];
        db.upsert_session(&parsed, 0, 0).unwrap();
        (dir, db)
    }

    /// A run spawned by `parent`, carrying one message that the fixture's `hello` query matches
    /// so a search returns the fragment and its root together.
    fn insert_subagent_session(db: &Db, id: &str, parent: &str, agent_label: &str) {
        let path = format!("/{}.jsonl", id.replace(['/', ':'], "_"));
        let mut parsed = minimal_record(Provider::Claude, Path::new(&path), String::new());
        parsed.session.id = id.to_string();
        parsed.session.provider_session_id = id.to_string();
        parsed.session.cwd = Some(FIXTURE_PROJECT.to_string());
        parsed.session.parent_session_id = Some(parent.to_string());
        parsed.session.agent_label = Some(agent_label.to_string());
        parsed.messages = vec![Message {
            seq: 0,
            role: Role::User,
            ts: None,
            tool_name: None,
            kind: crate::models::MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: "delta hello from a spawned run".to_string(),
            provenance: Default::default(),
        }];
        db.upsert_session(&parsed, 0, 0).unwrap();
    }

    /// A2 from the longitudinal hook-marker audit: one exact search finds the messages, but the
    /// deduplicated metadata beside them could not say whether a hit came from a root session, a
    /// spawned run, or a compaction fragment. Establishing that needed a second query against the
    /// sessions table, so the include did not answer the question it exists to answer. Both
    /// fields are already indexed and already published on the full session record.
    #[test]
    fn normalized_session_metadata_names_the_spawning_session_and_agent() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        insert_subagent_session(
            &db,
            "claude:test1/agent-acompact-01",
            "claude:test1",
            "acompact",
        );

        let out = search_messages_value(
            &json!({ "query": "hello", "include": ["normalized_session_metadata"] }),
            &config,
            &db,
        );
        let included = &out["included"]["normalized_session_metadata"];

        // A root run names no spawner. Both fields are present and null rather than absent, so a
        // consumer reads one shape for every session instead of branching on key presence.
        // Assert presence separately: indexing a missing key also yields null.
        let root = included["claude:test1"].as_object().expect("root metadata");
        assert_eq!(
            root.get("parent_session_id"),
            Some(&Value::Null),
            "{root:?}"
        );
        assert_eq!(root.get("agent_label"), Some(&Value::Null), "{root:?}");

        // The spawned run names both, so a caller groups or excludes fragments from this one
        // response rather than issuing a follow-up query per session id.
        let fragment = &included["claude:test1/agent-acompact-01"];
        assert_eq!(fragment["parent_session_id"], "claude:test1");
        assert_eq!(fragment["agent_label"], "acompact");
    }

    fn insert_list_session(db: &Db, id: &str, updated_at: &str) {
        let path = format!("/{id}.jsonl");
        let mut parsed = minimal_record(Provider::Claude, Path::new(&path), String::new());
        parsed.session.id = id.to_string();
        parsed.session.provider_session_id = id.to_string();
        parsed.session.title = Some("Proj".to_string());
        parsed.session.updated_at = crate::util::parse_datetime(updated_at);
        db.upsert_session(&parsed, 0, 0).unwrap();
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn structured(response: ToolResponse) -> Value {
        response
            .structured_content
            .expect("tool returns authoritative structured content")
    }

    #[derive(Clone)]
    #[allow(deprecated)]
    struct OfficialRootsClient {
        roots_capability: bool,
        roots_list_changed: bool,
        responses: Arc<
            tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<rmcp::model::ListRootsResult>>,
        >,
        request_count: Arc<AtomicUsize>,
        active_requests: Arc<AtomicUsize>,
        maximum_active_requests: Arc<AtomicUsize>,
    }

    impl rmcp::ClientHandler for OfficialRootsClient {
        #[allow(deprecated)]
        fn get_info(&self) -> rmcp::model::ClientInfo {
            let capabilities = if self.roots_capability {
                let capabilities = rmcp::model::ClientCapabilities::builder().enable_roots();
                if self.roots_list_changed {
                    capabilities.enable_roots_list_changed().build()
                } else {
                    capabilities.build()
                }
            } else {
                rmcp::model::ClientCapabilities::default()
            };
            rmcp::model::ClientInfo::new(
                capabilities,
                rmcp::model::Implementation::new("roots-test-client", "1"),
            )
        }

        #[allow(deprecated)]
        async fn list_roots(
            &self,
            _context: rmcp::service::RequestContext<rmcp::RoleClient>,
        ) -> Result<rmcp::model::ListRootsResult, rmcp::ErrorData> {
            self.request_count.fetch_add(1, Ordering::AcqRel);
            let active = self.active_requests.fetch_add(1, Ordering::AcqRel) + 1;
            self.maximum_active_requests
                .fetch_max(active, Ordering::AcqRel);
            let response = self.responses.lock().await.recv().await.ok_or_else(|| {
                rmcp::ErrorData::internal_error("roots test response channel closed", None)
            });
            self.active_requests.fetch_sub(1, Ordering::AcqRel);
            response
        }
    }

    #[allow(deprecated)]
    fn official_roots_client(
        roots_capability: bool,
    ) -> (
        OfficialRootsClient,
        tokio::sync::mpsc::UnboundedSender<rmcp::model::ListRootsResult>,
    ) {
        official_roots_client_with_capabilities(roots_capability, roots_capability)
    }

    #[allow(deprecated)]
    fn official_roots_client_with_capabilities(
        roots_capability: bool,
        roots_list_changed: bool,
    ) -> (
        OfficialRootsClient,
        tokio::sync::mpsc::UnboundedSender<rmcp::model::ListRootsResult>,
    ) {
        let (response_tx, response_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            OfficialRootsClient {
                roots_capability,
                roots_list_changed,
                responses: Arc::new(tokio::sync::Mutex::new(response_rx)),
                request_count: Arc::new(AtomicUsize::new(0)),
                active_requests: Arc::new(AtomicUsize::new(0)),
                maximum_active_requests: Arc::new(AtomicUsize::new(0)),
            },
            response_tx,
        )
    }

    struct GatedServerTransport<T> {
        inner: T,
        enabled: Arc<AtomicBool>,
        response_attempted: Arc<tokio::sync::Semaphore>,
        release_response: Arc<tokio::sync::Semaphore>,
    }

    impl<T> rmcp::transport::Transport<rmcp::RoleServer> for GatedServerTransport<T>
    where
        T: rmcp::transport::Transport<rmcp::RoleServer>,
    {
        type Error = T::Error;

        fn send(
            &mut self,
            item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let gate_response = self.enabled.load(Ordering::Acquire)
                && matches!(
                    item,
                    rmcp::model::ServerJsonRpcMessage::Response(_)
                        | rmcp::model::ServerJsonRpcMessage::Error(_)
                );
            let response_attempted = Arc::clone(&self.response_attempted);
            let release_response = Arc::clone(&self.release_response);
            let send = self.inner.send(item);
            async move {
                if gate_response {
                    response_attempted.add_permits(1);
                    let permit = release_response
                        .acquire()
                        .await
                        .expect("test gate remains open");
                    permit.forget();
                }
                send.await
            }
        }

        fn receive(
            &mut self,
        ) -> impl Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>> + Send
        {
            self.inner.receive()
        }

        fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.inner.close()
        }
    }

    struct FailingResponseTransport<T> {
        inner: T,
        enabled: Arc<AtomicBool>,
        response_attempted: Arc<tokio::sync::Semaphore>,
    }

    impl<T> rmcp::transport::Transport<rmcp::RoleServer> for FailingResponseTransport<T>
    where
        T: rmcp::transport::Transport<rmcp::RoleServer, Error = std::io::Error>,
    {
        type Error = std::io::Error;

        fn send(
            &mut self,
            item: rmcp::service::TxJsonRpcMessage<rmcp::RoleServer>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            let fail_response = self.enabled.load(Ordering::Acquire)
                && matches!(
                    item,
                    rmcp::model::ServerJsonRpcMessage::Response(_)
                        | rmcp::model::ServerJsonRpcMessage::Error(_)
                );
            let response_attempted = Arc::clone(&self.response_attempted);
            let send = self.inner.send(item);
            async move {
                if fail_response {
                    response_attempted.add_permits(1);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "injected MCP response delivery failure",
                    ));
                }
                send.await
            }
        }

        fn receive(
            &mut self,
        ) -> impl Future<Output = Option<rmcp::service::RxJsonRpcMessage<rmcp::RoleServer>>> + Send
        {
            self.inner.receive()
        }

        fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            self.inner.close()
        }
    }

    fn search_messages_value(args: &Value, config: &Config, db: &Db) -> Value {
        structured(tool_search_messages(args, config, db).unwrap())
    }

    #[test]
    #[allow(deprecated)]
    fn typed_rmcp_roots_reject_non_file_uris_with_the_parameter_location() {
        let error = parse_typed_mcp_roots(&rmcp::model::ListRootsResult::new(vec![
            rmcp::model::Root::new("https://example.com/not-local"),
        ]))
        .unwrap_err()
        .to_string();

        assert_eq!(error, "roots[0].uri must use the file scheme");
    }

    #[test]
    fn mcp_numeric_arguments_reject_out_of_range_values_instead_of_clamping() {
        let maximum = max_mcp_numeric_usize();
        let too_large = u64::try_from(maximum).unwrap().checked_add(1).unwrap();
        let error =
            mcp_nonnegative_usize_arg(&json!({ "offset": too_large }), "offset", 0).unwrap_err();
        assert!(error.contains("offset"), "{error}");
        assert!(error.contains(&maximum.to_string()), "{error}");
        assert!(error.contains(&too_large.to_string()), "{error}");

        let error = mcp_positive_usize_arg(&json!({ "preview_chars": 0 }), "preview_chars", 10)
            .unwrap_err();
        assert!(error.contains("preview_chars"), "{error}");
        assert!(error.contains("1 through"), "{error}");
        assert!(error.contains("got 0"), "{error}");

        let error = mcp_nonnegative_i64_arg(&json!({ "context": -2 }), "context", 0).unwrap_err();
        assert!(error.contains("context"), "{error}");
        assert!(error.contains("non-negative"), "{error}");
        assert!(error.contains("-2"), "{error}");
    }

    fn call_tool(name: &str, arguments: Value, config: &Config, db: &Db) -> Value {
        match dispatch_tool_cancellable(name, &arguments, config, db, None) {
            Ok(content) => {
                let mut result = json!({
                    "content": [{ "type": "text", "text": content.text }]
                });
                if let Some(structured) = content.structured_content {
                    result["structuredContent"] = structured;
                }
                json!({ "result": result })
            }
            Err(error) => json!({
                "result": {
                    "isError": true,
                    "content": [{ "type": "text", "text": error }]
                }
            }),
        }
    }

    /// Isolated config for the fixture index. Provider discovery paths are pinned to an empty
    /// directory under `dir`: `Config::default()` resolves them to the REAL user home
    /// (`~/.claude/projects`, `~/.codex/sessions`, ...), so without this the fixture's status
    /// output would depend on whatever transcripts happen to exist on the machine running the
    /// tests. The fixture indexes one synthetic session whose `source_path` (`/x/s.jsonl`) is
    /// not on disk, so a hermetic run must discover nothing.
    fn config_for_fixture(dir: &tempfile::TempDir) -> Config {
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        let sources = dir.path().join("empty-sources");
        std::fs::create_dir_all(&sources).unwrap();
        let sources = vec![sources.to_string_lossy().into_owned()];
        for provider in [
            &mut config.providers.claude,
            &mut config.providers.claude_desktop,
            &mut config.providers.codex,
            &mut config.providers.cursor,
            &mut config.providers.antigravity,
            &mut config.providers.pi,
            &mut config.providers.aistudio,
            &mut config.providers.gemini_cli,
        ] {
            provider.paths = sources.clone();
        }
        config
    }

    const MESSAGE_SEARCH_MODE_CASES: [(MessageSearchMode, &str); 3] = [
        (MessageSearchMode::Literal, "hello"),
        (MessageSearchMode::Regex, "h.llo"),
        (MessageSearchMode::Fuzzy, "helo"),
    ];

    fn with_search_mode(mut args: Value, mode: MessageSearchMode, pattern: &str) -> Value {
        let map = args.as_object_mut().expect("test args must be an object");
        map.insert("query".to_string(), json!(pattern));
        map.insert(
            "query_mode".to_string(),
            json!(if mode == MessageSearchMode::Literal {
                "literal"
            } else {
                mode.as_str()
            }),
        );
        args
    }

    #[test]
    fn search_messages_returns_canonical_structured_content_with_a_bounded_text_summary() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let response = tool_search_messages(&json!({ "query": "hello" }), &config, &db).unwrap();
        let structured = response
            .structured_content
            .as_ref()
            .expect("search_messages returns authoritative structured content");
        assert_eq!(
            structured
                .as_object()
                .expect("canonical response object")
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            [
                // Stated once for the document rather than at every offset site: three per
                // result before, one now.
                "coordinate_unit",
                "effective_request",
                "included",
                "page",
                "response_schema_version",
                "results"
            ]
        );
        assert_eq!(structured["page"]["returned"], 2);
        assert_eq!(
            structured["results"][0]["message_ref"]["session_id"],
            "claude:test1"
        );
        assert_eq!(
            structured["results"][0]["match"]["literal_occurrence"]["text"], "hello",
            "MCP must preserve the exact matched literal in structured content"
        );
        assert!(response
            .text
            .contains("structuredContent is the authoritative response"));
        assert!(response.text.contains("2 results at offset 0"));
        assert!(response.text.contains("match content["));
        assert!(response.text.contains("full message: get_session("));
        assert!(
            response.text.len() < serde_json::to_string_pretty(structured).unwrap().len(),
            "the text channel must not duplicate the full structured response"
        );
    }

    #[test]
    fn search_messages_compact_context_cannot_bypass_the_field_view_budget() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let large_neighbor = "neighbor ".repeat(4_096);
        let mut parsed = minimal_record(
            Provider::Claude,
            Path::new("/x/context-budget.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:context-budget".into();
        parsed.session.provider_session_id = "context-budget".into();
        parsed.messages = vec![
            Message {
                seq: 0,
                role: Role::User,
                ts: None,
                tool_name: None,
                kind: crate::models::MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: "bounded context anchor needle".into(),
                provenance: Default::default(),
            },
            Message {
                seq: 1,
                role: Role::Assistant,
                ts: None,
                tool_name: None,
                kind: crate::models::MessageKind::Conversation,
                tool_call_id: None,
                is_compaction: false,
                content: large_neighbor,
                provenance: Default::default(),
            },
        ];
        db.upsert_session(&parsed, 0, 0).unwrap();

        let response = tool_search_messages(
            &json!({
                "query": "needle",
                "session_id": "claude:context-budget",
                "context_after": 1,
                "field_view": {"kind": "max_chars", "max_chars": 32}
            }),
            &config,
            &db,
        )
        .unwrap();
        let context_view = &response.structured_content.as_ref().unwrap()["results"][0]["context"]
            ["messages_after"][0]["presentation"]["field_view"];
        assert_eq!(context_view["text"].as_str().unwrap().chars().count(), 32);
        assert_eq!(
            context_view["additional_field_text"], "after",
            "the canonical context view must state that more message content follows"
        );
        assert!(
            context_view.get("content").is_none(),
            "context must not retain a second unbounded content field"
        );
    }

    #[test]
    fn search_messages_enriches_with_session_metadata_and_paginates() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        // "hello" matches the two user turns. Normalized session data is deduplicated behind the
        // named include, while every result carries one exact message reference.
        let out = search_messages_value(
            &json!({
                "query": "hello",
                "include": ["normalized_session_metadata"]
            }),
            &config,
            &db,
        );
        assert_eq!(out["page"]["returned"], 2);
        assert!(out["page"]["next_offset"].is_null());
        let result = &out["results"][0];
        assert_eq!(result["message_ref"]["session_id"], "claude:test1");
        assert!(result["message_ref"]["message_seq"].is_number());
        let session_meta = &out["included"]["normalized_session_metadata"]["claude:test1"];
        assert_eq!(session_meta["provider_session_id"], "test1");
        assert_eq!(session_meta["cwd"], FIXTURE_PROJECT);
        assert_eq!(session_meta["repo_root"], FIXTURE_PROJECT);
        assert_eq!(session_meta["title"], "Proj");
        assert_eq!(session_meta["message_count"], 3);
        assert!(
            session_meta.get("source_path").is_none(),
            "search pages keep ingestion provenance out of repeated metadata"
        );

        // Page size 1: the first page reports a next_offset, the last page reports none.
        let p0 = search_messages_value(
            &json!({ "query": "hello", "limit": 1, "offset": 0 }),
            &config,
            &db,
        );
        assert_eq!(p0["page"]["returned"], 1);
        assert_eq!(p0["page"]["next_offset"], 1);
        let p1 = search_messages_value(
            &json!({ "query": "hello", "limit": 1, "offset": 1 }),
            &config,
            &db,
        );
        assert_eq!(p1["page"]["returned"], 1);
        assert!(p1["page"]["next_offset"].is_null());
    }

    /// Report the emitted JSON type name serde produced, in JSON Schema's vocabulary.
    fn json_type_name(value: &Value) -> &'static str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// True when `declared` (a `type` keyword: one name or a list of names) admits `emitted`.
    /// JSON Schema counts a whole-numbered value as both `integer` and `number`.
    fn schema_type_admits(declared: &Value, emitted: &'static str) -> bool {
        let admits = |name: &str| name == emitted || (name == "number" && emitted == "integer");
        match declared {
            Value::String(name) => admits(name),
            Value::Array(names) => names.iter().filter_map(Value::as_str).any(admits),
            _ => true,
        }
    }

    /// Collect every place a runtime response contradicts its own published output schema:
    /// a key no closed schema declares, or a value whose JSON type the declared `type` excludes.
    ///
    /// Deliberately narrower than JSON Schema validation. It answers the two questions a
    /// hand-written schema actually gets wrong — is every emitted key named, and is a nullable
    /// field declared nullable — and ignores `required`, `const`, `enum`, and numeric bounds.
    /// An open object (`additionalProperties` absent, or itself a schema) is descended through
    /// that value schema; a `oneOf`/`anyOf` union passes when any one alternative accepts every
    /// key, and otherwise reports the closest alternative's findings.
    ///
    /// `O(emitted keys × union alternatives)` time, `O(nesting depth)` retained stack.
    fn output_schema_violations(value: &Value, schema: &Value, path: &str, out: &mut Vec<String>) {
        if let Some(declared) = schema.get("type") {
            let emitted = json_type_name(value);
            if !schema_type_admits(declared, emitted) {
                out.push(format!(
                    "{path}: emitted {emitted}, schema declares {declared}"
                ));
            }
        }
        match value {
            Value::Object(fields) => {
                let declared = schema.get("properties");
                let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
                for (key, child) in fields {
                    let child_path = format!("{path}.{key}");
                    if let Some(child_schema) = declared.and_then(|properties| properties.get(key))
                    {
                        output_schema_violations(child, child_schema, &child_path, out);
                    } else if closed {
                        out.push(child_path);
                    } else if let Some(values) =
                        schema.get("additionalProperties").filter(|s| s.is_object())
                    {
                        output_schema_violations(child, values, &child_path, out);
                    }
                }
            }
            Value::Array(items) => {
                if let Some(item_schema) = schema.get("items") {
                    for (index, item) in items.iter().enumerate() {
                        output_schema_violations(
                            item,
                            item_schema,
                            &format!("{path}[{index}]"),
                            out,
                        );
                    }
                }
            }
            _ => {}
        }
        // Sibling keywords are conjunctive, so a union narrows the checks above rather than
        // replacing them, and it is evaluated after them. Checking only the alternatives would
        // accept every key vacuously here, because `add_not_ready_output_alternatives` writes
        // alternatives that carry `required` alone.
        let Some(alternatives) = schema
            .get("oneOf")
            .or_else(|| schema.get("anyOf"))
            .and_then(Value::as_array)
        else {
            return;
        };
        let mut closest: Option<Vec<String>> = None;
        for alternative in alternatives {
            let mut misses = Vec::new();
            output_schema_violations(value, alternative, path, &mut misses);
            if misses.is_empty() {
                return;
            }
            if closest
                .as_ref()
                .is_none_or(|best| misses.len() < best.len())
            {
                closest = Some(misses);
            }
        }
        out.extend(closest.unwrap_or_default());
    }

    /// Sweep every client limit over every advertised tool.
    ///
    /// The rows, their measurements and their provenance live in
    /// [`crate::mcp_schema_budget`], which the hidden `aise mcp schema-budget` command runs
    /// against the shipped binary. This test runs the same sweep against the same catalogue
    /// builder so a developer fails locally before the release gate does, and adds the
    /// no-regression ceilings above, which only make sense against a checked-out tree.
    #[test]
    fn every_tool_stays_inside_the_limits_its_clients_enforce() {
        use crate::mcp_schema_budget::{
            ceiling_for, codex_visible_schema, compact_len, evaluate, schema_depth, Status,
        };

        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let listed = handle_tools_list(Some(json!(1)), &config);
        let tools = listed["result"]["tools"].as_array().expect("tools list");

        // A sweep over an empty catalogue satisfies every rule below without measuring anything,
        // which is the shape of the false clean bill this whole package exists to prevent.
        assert_eq!(
            tools.len(),
            8,
            "expected the full eight-tool catalogue, found {}",
            tools.len()
        );

        // The filter is load-bearing: reduced to `{}` a schema fits every budget. Prove it kept
        // the property names before trusting any measurement taken through it.
        let search = tools
            .iter()
            .find(|tool| tool["name"] == "search_messages")
            .expect("search_messages is served");
        let measured = compact_len(&codex_visible_schema(&search["inputSchema"]));
        assert!(
            measured > 2_000,
            "codex_visible_schema collapsed search_messages to {measured} B; it must keep \
             `properties` KEYS and recurse only into their values, or every budget assertion \
             here silently passes"
        );

        let failures: Vec<String> = evaluate(tools, false)
            .into_iter()
            .filter(|finding| finding.status == Status::Fail)
            .map(|finding| {
                format!(
                    "{} [{}]: {} {} against {} ({})",
                    finding.limit.name,
                    finding.tool,
                    finding.measured,
                    finding.limit.unit,
                    finding.limit.budget,
                    finding.evidence
                )
            })
            .collect();
        assert!(
            failures.is_empty(),
            "enforced client limits breached: {failures:#?}"
        );

        for tool in tools {
            let name = tool["name"].as_str().expect("tool name");
            let (byte_ceiling, depth_ceiling) = ceiling_for(name).unwrap_or_else(|| {
                panic!(
                    "{name} has no recorded artifact ceiling; add one to \
                     mcp_schema_budget::EMITTED_ARTIFACT_CEILINGS so a new tool cannot grow \
                     unwatched"
                )
            });

            let bytes = compact_len(&codex_visible_schema(&tool["inputSchema"]));
            assert!(
                bytes <= byte_ceiling,
                "{name} inputSchema grew to {bytes} B as Codex counts it, above its recorded \
                 {byte_ceiling} B ceiling. Past 5,000 B Codex deletes every parameter \
                 description with no marker to the model."
            );

            let depth = schema_depth(&tool["outputSchema"]);
            assert!(
                depth <= depth_ceiling,
                "{name} outputSchema nests {depth} deep, above its recorded {depth_ceiling} \
                 ceiling; a conforming client may refuse it"
            );
        }
    }

    /// Two emissions must be byte-identical, which is what makes prompt-cache hits possible.
    #[test]
    fn two_tools_list_emissions_are_byte_identical() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let first = handle_tools_list(Some(json!(1)), &config);
        let second = handle_tools_list(Some(json!(1)), &config);
        assert_eq!(
            serde_json::to_string(&first).expect("first emission serializes"),
            serde_json::to_string(&second).expect("second emission serializes"),
            "tools/list is not deterministic; the specification asks servers to return tools in a \
             deterministic order because it improves LLM prompt cache hit rates"
        );
    }

    /// A published `outputSchema` is a promise a validating MCP client enforces. A closed object
    /// says the response carries no other key, and a non-nullable `type` says the field is never
    /// null, so a schema that renames a field or forgets that one is optional makes a correct
    /// response fail validation at the caller with no server-side symptom to debug from.
    #[test]
    fn search_messages_output_schema_matches_every_key_and_type_the_runtime_emits() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let tools = handle_tools_list(Some(json!(1)), &config);
        let schema = tools["result"]["tools"]
            .as_array()
            .expect("tools list")
            .iter()
            .find(|tool| tool["name"] == "search_messages")
            .expect("search_messages is served")["outputSchema"]
            .clone();
        // A missing or open schema would make every assertion below vacuously true, so prove the
        // published document is the closed one before trusting a clean result from it.
        assert_eq!(schema["additionalProperties"], json!(false), "{schema}");
        assert!(schema["properties"]["included"].is_object(), "{schema}");

        // Payload groups are opt-in, so an undeclared key inside one is never emitted by a
        // default call. Request the include that carries normalized session metadata.
        let out = search_messages_value(
            &json!({ "query": "hello", "include": ["normalized_session_metadata"] }),
            &config,
            &db,
        );

        let mut violations = Vec::new();
        output_schema_violations(&out, &schema, "structuredContent", &mut violations);
        assert!(
            violations.is_empty(),
            "response contradicts the published outputSchema: {violations:?}"
        );
    }

    #[test]
    fn search_sessions_offset_continues_one_fixed_ranking_and_discloses_mutability() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        insert_list_session(&db, "claude:test2", "2026-01-02T00:00:00Z");
        insert_list_session(&db, "claude:test3", "2026-01-03T00:00:00Z");

        let all = call_tool(
            "search_sessions",
            json!({ "query": "Proj", "limit": 0 }),
            &config,
            &db,
        )["result"]["structuredContent"]
            .clone();
        let first = call_tool(
            "search_sessions",
            json!({ "query": "Proj", "limit": 1, "offset": 0 }),
            &config,
            &db,
        )["result"]["structuredContent"]
            .clone();
        let second = call_tool(
            "search_sessions",
            json!({ "query": "Proj", "limit": 1, "offset": 1 }),
            &config,
            &db,
        )["result"]["structuredContent"]
            .clone();

        assert_eq!(first["sessions"][0]["id"], all["sessions"][0]["id"]);
        assert_eq!(second["sessions"][0]["id"], all["sessions"][1]["id"]);
        assert_eq!(first["next_offset"], 1);
        assert_eq!(second["next_offset"], 2);
        assert_eq!(first["pagination"]["offset"], 0);
        assert_eq!(first["pagination"]["consistency"], "per-call");
        assert!(first["pagination"]["order"]
            .as_str()
            .is_some_and(|order| order.contains("score")));

        let tool = tool_input_schema(&config, "search_sessions");
        assert_eq!(tool["inputSchema"]["properties"]["offset"]["default"], 0);
        assert!(tool["inputSchema"]["properties"]["offset"]["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("fixed index") && description.contains("change between calls")
            }));
        assert!(
            tool["outputSchema"]["properties"]["next_offset"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("same fixed index"))
        );
    }

    #[test]
    fn search_messages_schema_lists_configured_purpose_names_without_hardcoding() {
        let (dir, _db) = fixture();
        let mut config = config_for_fixture(&dir);
        config.search.purposes.insert(
            "incident-review".to_string(),
            crate::config::PurposeDefinition {
                version: std::num::NonZeroU32::new(3).unwrap(),
                operation: crate::config::SearchOperation::MessageSearch,
                preferences: Default::default(),
            },
        );

        let tool = tool_input_schema(&config, "search_messages");
        assert_eq!(
            tool["inputSchema"]["properties"]["purpose"]["enum"],
            json!(["incident-review"])
        );
        assert!(tool["inputSchema"]["properties"]["purpose"]["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("incident-review") && description.contains("configured")
            }));
    }

    #[test]
    fn search_messages_runtime_variants_conform_to_the_closed_output_schema() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let schema = search_messages_output_schema();

        for args in [
            json!({ "query": "hello" }),
            json!({ "query": "helo", "query_mode": "fuzzy" }),
            json!({
                "field": "tool_name",
                "all_results": true,
                "receipt_level": "summary"
            }),
            json!({
                "query": "alpha",
                "context": 1,
                "include": [
                    "normalized_session_metadata",
                    "parsed_references",
                    "raw_provider_metadata",
                    "runtime_diagnostics"
                ],
                "receipt_level": "full"
            }),
        ] {
            let output = search_messages_value(&args, &config, &db);
            validate_schema_value(
                &output,
                &schema,
                &schema,
                "search_messages",
                "structuredContent",
            )
            .unwrap_or_else(|error| panic!("{error}\n{output:#}"));
        }
    }

    #[test]
    fn search_messages_explain_reports_regex_planner_diagnostics() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = search_messages_value(
            &json!({
                "query": "hello",
                "query_mode": "regex",
                "receipt_level": "summary",
                "limit": 1
            }),
            &config,
            &db,
        );

        let explain = &out["receipt"]["search_explanation"];
        assert!(explain["corpus"].as_i64().unwrap() >= 1);
        assert!(explain["prefilter"].as_str().unwrap().contains("hel"));
        assert!(explain["candidates"].as_i64().unwrap() >= 1);
        assert!(explain["prefilter_skipped"].is_null());
        assert!(
            out["receipt"].get("parameter_origins").is_none(),
            "summary omits detailed origins"
        );

        let full = search_messages_value(
            &json!({
                "query": "hello",
                "query_mode": "regex",
                "receipt_level": "full",
                "limit": 1
            }),
            &config,
            &db,
        );
        let origins = full["receipt"]["parameter_origins"]
            .as_object()
            .expect("full receipt includes resolved parameter origins");
        assert_eq!(origins["result_extent"]["source"], "explicit");
        assert_eq!(
            origins["context_messages_before"]["source"],
            "typed-default"
        );
        assert_eq!(origins["lines_per_message"]["source"], "surface-config");
        assert_eq!(origins["lines_per_message"]["surface"], "mcp");
        assert_eq!(origins["receipt_level"]["source"], "explicit");
        assert!(full["receipt"]["ordered_digest"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:")));
    }

    #[test]
    fn search_messages_path_filter_context_window_and_mutual_exclusion() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        for (mode, pattern) in MESSAGE_SEARCH_MODE_CASES {
            // A path_prefix not containing the session filters it out entirely.
            let none = search_messages_value(
                &with_search_mode(
                    json!({ "workspace_path_prefix": FIXTURE_OTHER_PROJECT }),
                    mode,
                    pattern,
                ),
                &config,
                &db,
            );
            assert_eq!(
                none["page"]["returned"], 0,
                "{mode:?}: path prefix excludes session"
            );

            // A matching absolute path_prefix returns the session's user messages. The fixture cwd
            // does not exist on disk, so this also exercises the lexical-absolute fallback path.
            let scoped = search_messages_value(
                &with_search_mode(
                    json!({
                        "workspace_path_prefix": FIXTURE_PROJECT,
                        "role": "user",
                        "include": ["normalized_session_metadata"]
                    }),
                    mode,
                    pattern,
                ),
                &config,
                &db,
            );
            assert_eq!(
                scoped["page"]["returned"], 2,
                "{mode:?}: path prefix includes session"
            );
            let result = &scoped["results"][0];
            assert_eq!(result["message_metadata"]["provider"], "claude");
            assert_eq!(
                scoped["included"]["normalized_session_metadata"]["claude:test1"]["cwd"],
                FIXTURE_PROJECT
            );
            assert_eq!(
                scoped["included"]["normalized_session_metadata"]["claude:test1"]["title"],
                "Proj"
            );
            if mode == MessageSearchMode::Fuzzy {
                assert!(result["match"]["fuzzy_score"].as_u64().unwrap() > 0);
            }
        }

        // Context separates neighboring messages from the matched result and preserves their
        // exact identities while applying the resolved presentation budget.
        let ctx = search_messages_value(&json!({ "query": "alpha", "context": 1 }), &config, &db);
        let after = &ctx["results"][0]["context"]["messages_after"];
        assert_eq!(after[0]["message_ref"]["message_seq"], 1);
        assert_eq!(after[0]["message_metadata"]["provider"], "claude");

        // Non-exact modes require a query, and mode values are closed and explicit.
        assert!(tool_search_messages(&json!({ "query_mode": "regex" }), &config, &db).is_err());
        assert!(tool_search_messages(&json!({ "query_mode": "fuzzy" }), &config, &db).is_err());
        assert!(tool_search_messages(
            &json!({ "query": "hello", "query_mode": "approximate" }),
            &config,
            &db
        )
        .is_err());
    }

    #[test]
    fn search_messages_rejects_kind_and_kinds_together() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let error = tool_search_messages(
            &json!({
                "query": "hello",
                "kind": "conversation",
                "kinds": ["tool_result"],
                "limit": 1
            }),
            &config,
            &db,
        )
        .unwrap_err();
        assert_eq!(
            error,
            "kind and kinds cannot be used together; use kind for one class or kinds for several"
        );
    }

    #[test]
    fn search_messages_supports_fuzzy_matching_with_scores() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = search_messages_value(
            &json!({
                "query": "helo",
                "query_mode": "fuzzy",
                "role": "user",
                "limit": 2,
                "receipt_level": "summary"
            }),
            &config,
            &db,
        );

        assert_eq!(out["page"]["returned"], 2);
        let result = &out["results"][0];
        assert!(result["match"]["fuzzy_score"].as_u64().unwrap() > 0);
        assert!(result["presentation"]["field_view"]["text"]
            .as_str()
            .unwrap()
            .contains("hello"));
        assert!(out["receipt"]["search_explanation"]["prefilter_skipped"]
            .as_str()
            .unwrap()
            .contains("complete filtered corpus scored with bounded top-K retention"));
    }

    #[test]
    fn search_messages_exposes_selected_tool_argument_match_beyond_content_preview() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let command = format!("{}Trash{}", "x".repeat(855), "y".repeat(400));
        let mut parsed = minimal_record(
            Provider::Claude,
            Path::new("/x/evidence.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:evidence".into();
        parsed.session.provider_session_id = "evidence".into();
        parsed.messages = vec![Message {
            seq: 0,
            role: Role::Tool,
            ts: None,
            tool_name: Some("exec_command".into()),
            kind: crate::models::MessageKind::ToolCall,
            tool_call_id: Some("call-evidence".into()),
            is_compaction: false,
            content: serde_json::json!({
                "args": { "command": command },
                "kind": "tool_call",
                "tool_name": "exec_command"
            })
            .to_string(),
            provenance: Default::default(),
        }];
        db.upsert_session(&parsed, 0, 0).unwrap();

        let out = search_messages_value(
            &json!({
                "query": "Trash",
                "field": "tool_argument",
                "argument_path": "/command",
                "match_view": {"kind": "max_chars", "max_chars": 40},
                "field_view": {"kind": "max_chars", "max_chars": 80}
            }),
            &config,
            &db,
        );
        assert_eq!(out["effective_request"]["query"], "Trash");
        assert_eq!(
            out["page"]["has_more"].as_bool(),
            Some(out["page"]["next_offset"].is_number())
        );
        assert_eq!(
            out["effective_request"]["presentation"]["field_view"]["max_chars"],
            80
        );
        assert_eq!(out["effective_request"]["target"]["field"], "tool_argument");
        assert_eq!(
            out["effective_request"]["target"]["argument_path"],
            "/command"
        );
        let result = &out["results"][0];
        let field_view = &result["presentation"]["field_view"];
        assert!(!field_view["text"].as_str().unwrap().contains("Trash"));
        assert_eq!(field_view["additional_field_text"], "after");
        assert!(field_view["text"].as_str().unwrap().chars().count() <= 80);
        assert!(
            field_view["field_total_chars"]
                .as_u64()
                .is_some_and(|total| total > 80),
            "match evidence already established the exact selected-field total"
        );
        let match_view = &result["presentation"]["match_view"];
        assert!(match_view["text"].as_str().unwrap().contains("Trash"));
        assert_eq!(match_view["text"].as_str().unwrap().chars().count(), 40);
        assert_eq!(result["match"]["literal_occurrence"]["text"], "Trash");
        assert_eq!(
            result["match"]["literal_occurrence"]["field_start_char"],
            855
        );
        assert_eq!(
            result["match"]["literal_occurrence"]["field_end_char_exclusive"],
            860
        );

        let error = tool_search_messages(
            &json!({
                "query": "Trash",
                "field_view": {"kind": "max_chars", "max_chars": 0}
            }),
            &config,
            &db,
        )
        .unwrap_err();
        assert!(
            error.contains("field_view.max_chars")
                && error.contains("from 1 through")
                && error.contains("got 0"),
            "{error}"
        );
    }

    #[test]
    fn search_messages_mcp_covers_three_modes_by_three_fields() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let mut parsed = minimal_record(
            Provider::Claude,
            Path::new("/x/matrix.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:matrix".into();
        parsed.session.provider_session_id = "matrix".into();
        parsed.messages = vec![Message {
            seq: 0,
            role: Role::Tool,
            ts: None,
            tool_name: Some("exec_command".into()),
            kind: crate::models::MessageKind::ToolCall,
            tool_call_id: Some("call-1".into()),
            is_compaction: false,
            content: r#"{"args":{"cmd":"cargo test --workspace"},"kind":"tool_call","tool_name":"exec_command"}"#.into(),
            provenance: Default::default(),
        }];
        db.upsert_session(&parsed, 0, 0).unwrap();

        let cases = [
            ("content", "literal", "cargo test"),
            ("content", "regex", r"cargo\s+test"),
            ("content", "fuzzy", "crgo tst"),
            ("tool_name", "literal", "exec"),
            ("tool_name", "regex", r"^exec_"),
            ("tool_name", "fuzzy", "excmd"),
            ("tool_argument", "literal", "cargo test"),
            ("tool_argument", "regex", r"cargo\s+test"),
            ("tool_argument", "fuzzy", "crgo tst"),
        ];
        for (field, mode, query) in cases {
            let mut args = json!({
                "query": query,
                "field": field,
                "query_mode": mode,
                "kind": "tool_call",
                "session_id": "claude:matrix",
                "limit": 10,
                "receipt_level": "summary"
            });
            if field == "tool_argument" {
                args["argument_path"] = json!("/cmd");
            }
            let out = structured(
                tool_search_messages(&args, &config, &db)
                    .unwrap_or_else(|error| panic!("{field}/{mode}: {error}")),
            );
            assert_eq!(
                out["response_schema_version"], 1,
                "the response-contract version must not reuse the database schema version"
            );
            assert_eq!(out["page"]["returned"], 1, "{field}/{mode}: {out}");
            assert_eq!(
                out["results"][0]["message_ref"]["session_id"],
                "claude:matrix"
            );
            assert_eq!(out["results"][0]["message_ref"]["message_seq"], 0);
            assert_eq!(out["effective_request"]["query_mode"], mode);
            assert_eq!(out["effective_request"]["target"]["field"], field);
            if mode == "fuzzy" {
                assert!(out["results"][0]["match"]["fuzzy_score"]
                    .as_u64()
                    .is_some_and(|score| score > 0));
                assert_eq!(out["page"]["ordering"], "fuzzy-relevance");
            } else {
                assert!(out["results"][0]["match"].get("fuzzy_score").is_none());
                assert_eq!(out["page"]["ordering"], "session-sequence");
            }
            assert!(out["receipt"]["search_explanation"].is_object());
        }
    }

    #[test]
    fn search_messages_validates_general_tool_argument_pointer() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool(
            "search_messages",
            json!({
                "query": "cargo",
                "field": "tool_argument",
                "argument_path": "cmd"
            }),
            &config,
            &db,
        );
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("RFC 6901")));
    }

    #[test]
    fn search_messages_supports_exact_session_id_and_seq_bounds() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = search_messages_value(
            &json!({
                "query": "hello",
                "session_id": "claude:test1",
                "seq_from": 1,
                "seq_to": 2
            }),
            &config,
            &db,
        );
        assert_eq!(out["page"]["returned"], 1);
        assert_eq!(out["results"][0]["message_ref"]["message_seq"], 2);

        assert!(
            tool_search_messages(&json!({ "query": "hello", "seq_from": 1 }), &config, &db)
                .is_err(),
            "seq bounds are session-local and must require a session scope"
        );
        assert!(tool_search_messages(
            &json!({ "query": "hello", "session": "test" }),
            &config,
            &db
        )
        .is_err());
    }

    /// Return the advertised inputSchema for one tool, so schema-contract assertions read the
    /// exact JSON Schema an MCP client receives from tools/list.
    fn tool_input_schema(config: &Config, name: &str) -> Value {
        handle_tools_list(None, config)["result"]["tools"]
            .as_array()
            .expect("tools/list returns an array")
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("tool {name} is advertised"))
            .clone()
    }

    #[test]
    fn search_messages_order_newest_returns_last_n_chronologically() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        // The fixture session has seq 0,1,2. order=newest + limit 2 selects the LAST two by seq
        // (1 and 2, never 0), and returns them seq-ascending for a readable transcript.
        let out = search_messages_value(
            &json!({
                "session_id": "claude:test1",
                "match_window": "latest",
                "limit": 2
            }),
            &config,
            &db,
        );
        assert_eq!(out["page"]["returned"], 2, "{out}");
        assert_eq!(out["results"][0]["message_ref"]["message_seq"], 1);
        assert_eq!(out["results"][1]["message_ref"]["message_seq"], 2);

        // limit 1 = the single most recent message (seq 2), not the earliest.
        let last = search_messages_value(
            &json!({ "session_id": "claude:test1", "match_window": "latest", "limit": 1 }),
            &config,
            &db,
        );
        assert_eq!(last["page"]["returned"], 1);
        assert_eq!(last["results"][0]["message_ref"]["message_seq"], 2);
    }

    #[test]
    fn search_messages_order_newest_requires_session_id() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        // seq numbers are session-local, so newest is undefined without a single session scope.
        let error = tool_search_messages(&json!({ "match_window": "latest" }), &config, &db)
            .expect_err("newest without session_id must be rejected");
        assert!(error.contains("requires one session"), "{error}");

        // an unknown match-window value names the accepted set.
        let bad = tool_search_messages(
            &json!({ "session_id": "claude:test1", "match_window": "sideways" }),
            &config,
            &db,
        )
        .expect_err("unknown match window must be rejected");
        assert!(bad.contains("earliest") && bad.contains("latest"), "{bad}");
    }

    #[test]
    fn search_messages_schema_documents_match_window_and_forward_paging() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let tool = tool_input_schema(&config, "search_messages");

        let order = &tool["inputSchema"]["properties"]["match_window"];
        assert_eq!(
            order["enum"],
            json!(["earliest", "latest"]),
            "match_window advertises its two selection values"
        );
        // `latest` is scoped to one session and presented chronologically. That fact used to be
        // restated in this parameter's prose; it is now published once, verbatim from the
        // validator, as the conflict rule in `tool.description`. Assert it where it lives, so
        // the check follows the fact rather than the wording.
        let tool_doc = tool["description"].as_str().unwrap();
        assert!(
            tool_doc.contains("match_window=latest requires one session"),
            "the latest-match session scope reaches the model: {tool_doc}"
        );

        // Page continuation uses the response's explicit next offset, while focused message
        // expansion uses the separate session/message identity.
        assert!(
            tool_doc.contains("page.next_offset") && tool_doc.contains("next offset argument"),
            "search_messages description advertises offset continuation: {tool_doc}"
        );
        assert!(
            tool_doc.contains("get_session(session_id, message_seq)"),
            "search_messages description names the exact expansion call: {tool_doc}"
        );

        // The six message classes are the `kind` and `kinds` accepted values. They travel as
        // `enum`, which Codex models and VS Code preserves, so the caller learns them from the
        // schema rather than from a prose copy that can drift from it.
        let kind = &tool["inputSchema"]["properties"]["kind"];
        let advertised = kind["enum"].as_array().expect("kind advertises its values");
        for value in [
            "conversation",
            "compaction",
            "tool_call",
            "tool_result",
            "unknown",
            "harness_notice",
        ] {
            assert!(
                advertised.iter().any(|token| token == value),
                "kind advertises the {value:?} class: {advertised:?}"
            );
        }
        // What `enum` cannot say about them is said once, in the channel every client forwards.
        assert!(
            tool_doc.contains("harness_notice is what the harness told the agent"),
            "the one class whose meaning a token cannot carry is glossed: {tool_doc}"
        );
    }

    /// Every enum value the server advertises must be one its parser accepts, on every tool
    /// that advertises it.
    ///
    /// `SessionKind` got this check when it was added; `MessageKind`, `Provider`, `Role`, and
    /// `SearchField` never had one, though the `PATTERN` note on `MessageKind` makes the same
    /// promise ("it reaches the MCP schema through `message_kind_values`"). A schema offering a
    /// spelling the parser rejects is invisible until a caller is refused for using the value
    /// the tool told them to use.
    ///
    /// The unclassified-property assertion is the point of the design: a new enum property
    /// fails this test until it is either given a parser here or listed as presentation-only.
    /// Without it the test would silently stop covering whatever was added next.
    #[test]
    fn every_advertised_enum_value_parses_on_every_tool_that_offers_it() {
        use std::str::FromStr;

        type Parse = fn(&str) -> Result<(), String>;
        fn check<T: FromStr<Err = String>>(value: &str) -> Result<(), String> {
            T::from_str(value).map(|_| ())
        }
        let parsers: &[(&str, Parse)] = &[
            ("kind", check::<crate::models::MessageKind>),
            ("kinds", check::<crate::models::MessageKind>),
            ("session_kinds", check::<crate::models::SessionKind>),
            ("provider", check::<crate::models::Provider>),
            ("providers", check::<crate::models::Provider>),
            ("role", check::<crate::models::Role>),
            ("field", check::<crate::models::SearchField>),
            ("index_refresh", check::<crate::config::IndexRefresh>),
        ];
        // Advertised vocabularies with no Rust enum behind them: each is matched inline by the
        // handler that reads it, so there is no second list to drift from.
        let presentation_only = [
            "detail",
            "include",
            "match_window",
            "ordering",
            "query_mode",
            "receipt_level",
            "response_format",
            "selected_edge",
            "surface",
        ];

        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let listed = handle_tools_list(None, &config);
        let tools = listed["result"]["tools"]
            .as_array()
            .expect("tools/list returns an array");
        assert!(!tools.is_empty(), "no tools to inspect");

        let mut checked = 0usize;
        for tool in tools {
            let name = tool["name"].as_str().unwrap_or("<unnamed>");
            let Some(properties) = tool["inputSchema"]["properties"].as_object() else {
                continue;
            };
            for (property, schema) in properties {
                // A scalar enum lives at `enum`; an array's lives at `items.enum`.
                let Some(values) = schema["enum"]
                    .as_array()
                    .or_else(|| schema["items"]["enum"].as_array())
                else {
                    continue;
                };
                let Some((_, parse)) = parsers.iter().find(|(key, _)| key == property) else {
                    assert!(
                        presentation_only.contains(&property.as_str()),
                        "{name}.{property} advertises an enum with no parser and is not listed \
                         as presentation-only; classify it so this test keeps covering it"
                    );
                    continue;
                };
                assert!(
                    !values.is_empty(),
                    "{name}.{property} advertises an empty value set"
                );
                for value in values {
                    let value = value
                        .as_str()
                        .unwrap_or_else(|| panic!("{name}.{property} enum entry is not a string"));
                    parse(value).unwrap_or_else(|error| {
                        panic!("{name}.{property} advertises {value:?}, which its parser rejects: {error}")
                    });
                    checked += 1;
                }
            }
        }
        assert!(
            checked >= 20,
            "expected to check many advertised values, checked {checked} — the walk found \
             nothing, which passes vacuously"
        );
    }

    /// Both session tools advertise `session_kinds`, they advertise the SAME values, and every
    /// advertised value parses. A schema that offers a spelling the parser rejects is the
    /// drift this guards: the enum list is derived from `SessionKind` for exactly that reason,
    /// so this asserts the derivation holds end to end rather than trusting it.
    #[test]
    fn session_tools_advertise_the_session_classes_their_parser_accepts() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);

        let mut advertised = Vec::new();
        for tool_name in ["search_sessions", "list_sessions"] {
            let tool = tool_input_schema(&config, tool_name);
            let kinds = &tool["inputSchema"]["properties"]["session_kinds"];
            let values = kinds["items"]["enum"]
                .as_array()
                .unwrap_or_else(|| panic!("{tool_name} must advertise session_kinds values"))
                .clone();
            let doc = kinds["description"].as_str().unwrap();
            for value in values.iter().map(|value| value.as_str().unwrap()) {
                value
                    .parse::<crate::models::SessionKind>()
                    .unwrap_or_else(|error| {
                        panic!(
                            "{tool_name} advertises {value:?} but the parser rejects it: {error}"
                        )
                    });
                assert!(
                    doc.contains(value),
                    "{tool_name} advertises {value:?} without saying what it selects: {doc}"
                );
            }
            // `parent_session_id` is the other half of the link and has to be reachable from
            // both tools, or "every subagent of this session" is only answerable from one.
            assert!(
                tool["inputSchema"]["properties"]["parent_session_id"]["description"].is_string(),
                "{tool_name} must expose parent_session_id"
            );
            advertised.push(values);
        }
        assert_eq!(
            advertised[0], advertised[1],
            "the two session tools must offer one vocabulary, not two"
        );
        assert_eq!(
            Value::Array(advertised[0].clone()),
            json!(["user", "subagent"])
        );

        // A spelling that is not advertised is refused with a message naming what is.
        let error = search_filters_from_args(
            &json!({ "session_kinds": ["agent"] }),
            10,
            chrono::Utc::now(),
        )
        .expect_err("an unadvertised class must be rejected, not silently ignored");
        assert!(
            error.contains("user") && error.contains("subagent"),
            "the rejection must name the accepted values: {error}"
        );

        // A non-array is rejected by name rather than being coerced or dropped.
        let error = search_filters_from_args(
            &json!({ "session_kinds": "subagent" }),
            10,
            chrono::Utc::now(),
        )
        .expect_err("a bare string must not pass as a one-element array");
        assert!(error.contains("session_kinds"), "{error}");
    }

    #[test]
    fn get_session_seq_range_reads_absolute_message_range() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        // A seq_from/seq_to range reads exactly the messages in [0,1] without a larger head/tail.
        let out = parse(
            &tool_get_session(
                &json!({
                    "session_id": "claude:test1",
                    "seq_from": 0,
                    "seq_to": 1
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["session_id"], "claude:test1");
        assert_eq!(out["anchor_seq"], 0);
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{out}");
        assert_eq!(messages[0]["seq"], 0);
        assert_eq!(messages[0]["is_match"], true);
        assert_eq!(messages[1]["seq"], 1);
        assert_eq!(messages[1]["is_match"], false);

        // A later, non-overlapping chunk (seq_from = last seq + 1) reads the remainder.
        let next = parse(
            &tool_get_session(
                &json!({ "session_id": "claude:test1", "seq_from": 2 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        let next_messages = next["messages"].as_array().unwrap();
        assert_eq!(next_messages.len(), 1);
        assert_eq!(next_messages[0]["seq"], 2);
    }

    #[test]
    fn get_session_seq_range_validates_bounds_and_exclusivity() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let inverted = tool_get_session(
            &json!({ "session_id": "claude:test1", "seq_from": 2, "seq_to": 1 }),
            &config,
            &db,
        )
        .expect_err("from > to must be rejected");
        assert!(
            inverted.contains("seq_from must be <= seq_to"),
            "{inverted}"
        );

        let mixed = tool_get_session(
            &json!({ "session_id": "claude:test1", "seq_from": 0, "transcript_lines": 5 }),
            &config,
            &db,
        )
        .expect_err("range and transcript_lines are mutually exclusive selectors");
        assert!(
            mixed.contains("only one get_session output selector"),
            "{mixed}"
        );
    }

    #[test]
    fn get_session_schema_documents_seq_range_and_seq_cross_reference() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let tool = tool_input_schema(&config, "get_session");
        let properties = &tool["inputSchema"]["properties"];

        assert_eq!(properties["seq_from"]["type"], "integer");
        assert_eq!(properties["seq_to"]["type"], "integer");
        assert!(
            properties["seq_from"]["description"]
                .as_str()
                .unwrap()
                .contains("non-overlapping"),
            "seq_from doc explains non-overlapping chunk reads"
        );

        let message_seq_doc = properties["message_seq"]["description"].as_str().unwrap();
        assert!(
            message_seq_doc.contains("results[].message_ref.message_seq")
                && !message_seq_doc.contains("`seq` field"),
            "message_seq doc names the exact search_messages result path: {message_seq_doc}"
        );

        // task 35 guidance on the get_session description too.
        let tool_doc = tool["description"].as_str().unwrap();
        assert!(
            tool_doc.contains("seq_from = last returned seq + 1"),
            "get_session description advertises forward-paging: {tool_doc}"
        );
    }

    #[test]
    fn search_messages_include_refs_adds_structured_url_refs() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = search_messages_value(
            &json!({
                "query": "beta",
                "include": ["parsed_references"],
                "detail": "full"
            }),
            &config,
            &db,
        );
        let references = &out["results"][0]["included"]["parsed_references"];
        assert_eq!(references[0]["value"], "https://example.com/paper.pdf");
        assert_eq!(references[0]["host"], "example.com");

        let window = parse(
            &tool_get_session(
                &json!({
                    "session_id": "claude:test1",
                    "message_seq": 1,
                    "include_refs": true,
                    "response_format": "detailed"
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(window["messages"][0]["refs"][0]["host"], "example.com");
    }

    #[test]
    fn mcp_date_helpers_support_when_and_reject_mixed_bounds() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let (since_only, until_only) =
            parse_date_bounds(&json!({ "since": "2026-01" }), now).unwrap();
        assert_eq!(
            since_only.unwrap().to_rfc3339(),
            "2026-01-01T00:00:00+00:00"
        );
        assert!(until_only.is_none(), "`since` alone must stay open-ended");

        let (since_only, until_only) =
            parse_date_bounds(&json!({ "until": "2026-01" }), now).unwrap();
        assert!(since_only.is_none(), "`until` alone must stay open-ended");
        assert_eq!(
            until_only.unwrap().to_rfc3339(),
            "2026-01-31T23:59:59.999999999+00:00"
        );

        let (since, until) = parse_date_bounds(&json!({ "when": "2026-01" }), now).unwrap();
        assert_eq!(since.unwrap().to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(
            until.unwrap().to_rfc3339(),
            "2026-01-31T23:59:59.999999999+00:00"
        );
        assert!(
            parse_date_bounds(&json!({ "when": "2026-01", "since": "2026" }), now).is_err(),
            "`when` must stay mutually exclusive with since/until like CLI DateRange"
        );
        assert!(
            parse_date_bounds(&json!({ "when": "2026-01", "since": null }), now).is_ok(),
            "null optional MCP date args should behave like absent args"
        );
    }

    #[test]
    fn mcp_search_filters_normalize_path_and_share_since_until_when() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-15T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let filters = search_filters_from_args(
            &json!({
                "provider": "claude",
                "path_prefix": format!("{FIXTURE_PROJECT}{}.", std::path::MAIN_SEPARATOR),
                "when": "7d",
                "limit": 7
            }),
            20,
            now,
        )
        .unwrap();

        assert_eq!(filters.provider, Some(Provider::Claude));
        assert_eq!(
            filters.path_prefix,
            Some(normalize_path_prefix(&format!(
                "{FIXTURE_PROJECT}{}.",
                std::path::MAIN_SEPARATOR
            )))
        );
        assert_eq!(filters.limit, 7);
        assert_eq!(filters.until, Some(now));
        assert!(filters.since.is_some_and(|since| since < now));
    }

    #[test]
    fn get_session_returns_focused_message_window_when_message_seq_is_provided() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let anchor_only = parse(
            &tool_get_session(
                &json!({ "session_id": "claude:test1", "message_seq": 1 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        let anchor_msgs = anchor_only["messages"].as_array().unwrap();
        assert_eq!(
            anchor_msgs.len(),
            1,
            "default context is 0, so only the anchor is returned"
        );
        assert_eq!(anchor_msgs[0]["seq"], 1);
        assert_eq!(anchor_msgs[0]["is_match"], true);

        let out = parse(
            &tool_get_session(
                &json!({ "session_id": "test1", "message_seq": 1, "context": 1 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["session_id"], "claude:test1");
        assert_eq!(out["anchor_seq"], 1);
        assert_eq!(out["cwd"], FIXTURE_PROJECT);
        assert_eq!(out["repo"], FIXTURE_PROJECT);
        assert_eq!(out["title"], "Proj");
        assert_eq!(out["session_metadata"]["provider_session_id"], "test1");
        assert_eq!(out["session_metadata"]["source_path"], "/x/s.jsonl");
        assert_eq!(out["session_metadata"]["message_count"], 3);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "seq 0,1,2 in the window");
        assert!(msgs.iter().any(|m| m["seq"] == 1 && m["is_match"] == true));
        assert!(msgs.iter().any(|m| m["seq"] == 0 && m["is_match"] == false));
    }

    fn multiline_fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let mut parsed = minimal_record(Provider::Claude, Path::new("/x/m.jsonl"), String::new());
        parsed.session.id = "claude:multi1".to_string();
        parsed.session.provider_session_id = "multi1".to_string();
        parsed.transcript_text = "t0\nt1\nt2".to_string();
        parsed.messages = vec![Message {
            seq: 0,
            role: Role::Tool,
            ts: None,
            tool_name: Some("Bash".to_string()),
            kind: crate::models::MessageKind::ToolResult,
            tool_call_id: None,
            is_compaction: false,
            content: "needle first line\nsecond line\nthird line https://example.com/ref\nfinal exit status 0"
                .to_string(),
            provenance: Default::default(),
        }];
        db.upsert_session(&parsed, 0, 0).unwrap();
        (dir, db)
    }

    #[test]
    fn get_session_lines_per_message_caps_each_focused_message() {
        let (dir, db) = multiline_fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_get_session(
                &json!({
                    "session_id": "claude:multi1",
                    "message_seq": 0,
                    "response_format": "detailed",
                    "lines_per_message": -2
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(
            msgs[0]["content"], "third line https://example.com/ref\nfinal exit status 0",
            "negative lines_per_message keeps the tail of one message"
        );
        let extent = &msgs[0]["content_extent"];
        assert_eq!(extent["additional_field_text"], "before");
        // The unit is stated once for the whole document rather than at every offset site.
        assert_eq!(extent["coordinate_unit"], "unicode_scalar");
        assert_eq!(
            extent["field_end_char_exclusive"].as_u64().unwrap()
                - extent["field_start_char"].as_u64().unwrap(),
            msgs[0]["content"].as_str().unwrap().chars().count() as u64
        );
        assert!(
            extent.get("complete").is_none()
                && extent.get("omitted_start").is_none()
                && extent.get("omitted_end").is_none()
                && extent.get("returned_chars").is_none()
                && extent.get("original_chars").is_none(),
            "focused content uses one absolute range and one additional-text direction: {extent}"
        );
        validate_schema_value(
            &out,
            &get_session_output_schema(),
            &get_session_output_schema(),
            "get_session",
            "focused structuredContent",
        )
        .unwrap();

        let concise = parse(
            &tool_get_session(
                &json!({
                    "session_id": "claude:multi1",
                    "message_seq": 0,
                    "preview_chars": 10
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(
            concise["messages"][0]["content"], "needle fir",
            "a bounded view remains an exact field substring with honest coordinates"
        );
        assert_eq!(
            concise["messages"][0]["content_extent"]["additional_field_text"],
            "after"
        );

        let transcript_error = tool_get_session(
            &json!({ "session_id": "claude:multi1", "lines_per_message": 3 }),
            &config,
            &db,
        )
        .unwrap_err();
        assert!(
            transcript_error.contains("transcript_lines"),
            "transcript output must direct callers to transcript_lines: {transcript_error}"
        );

        let summary_error = tool_get_session(
            &json!({ "session_id": "claude:multi1", "summary": true, "lines_per_message": 3 }),
            &config,
            &db,
        )
        .unwrap_err();
        assert!(
            summary_error.contains("message_seq"),
            "summary output must direct callers to message_seq: {summary_error}"
        );
    }

    #[test]
    fn search_messages_lines_per_message_caps_hits_but_not_refs() {
        let (dir, db) = multiline_fixture();
        let config = config_for_fixture(&dir);

        let out = search_messages_value(
            &json!({
                "query": "needle",
                "include": ["parsed_references"],
                "lines_per_message": 2
            }),
            &config,
            &db,
        );
        let results = out["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0]["presentation"]["field_view"]["text"], "needle first line\nsecond line",
            "positive lines_per_message keeps the head of each hit"
        );
        let refs = results[0]["included"]["parsed_references"]
            .as_array()
            .unwrap();
        assert!(
            refs.iter().any(|r| r["value"] == "https://example.com/ref"),
            "refs come from full content even when the cap hides their line: {refs:?}"
        );
    }

    #[test]
    fn get_session_summary_optionally_includes_bounded_time_profile() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool(
            "get_session",
            json!({
                "session_id": "claude:test1",
                "summary": true,
                "include": ["time_profile"]
            }),
            &config,
            &db,
        );
        let summary = &response["result"]["structuredContent"];
        assert!(summary["time_profile"].is_object());
        assert!(summary["time_profile"]["messages"].is_number());

        let rejected = call_tool(
            "get_session",
            json!({"session_id": "claude:test1", "include": ["time_profile"]}),
            &config,
            &db,
        );
        assert_eq!(rejected["result"]["isError"], true);
    }

    #[test]
    fn get_session_summary_returns_compact_bundle() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_get_session(
                &json!({ "session_id": "claude:test1", "summary": true }),
                &config,
                &db,
            )
            .unwrap(),
        );

        assert_eq!(out["session"]["id"], "claude:test1");
        assert_eq!(out["user_intent"].as_array().unwrap().len(), 2);
        assert_eq!(out["refs"][0]["refs"][0]["host"], "example.com");

        let first = parse(
            &tool_get_session(
                &json!({ "session_id": "claude:test1", "summary": true, "summary_items": 1 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        let last = parse(
            &tool_get_session(
                &json!({ "session_id": "claude:test1", "summary": true, "summary_items": -1 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert!(
            first["user_intent"][0]["seq"].as_i64().unwrap()
                < last["user_intent"][0]["seq"].as_i64().unwrap()
        );

        let all = parse(
            &tool_get_session(
                &json!({ "session_id": "claude:test1", "summary": true, "summary_items": 0 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(all["truncated_evidence"], json!([]));

        assert!(out["next_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cmd| cmd
                .as_str()
                .unwrap()
                .contains("aise messages timeline claude:test1 --refs")));

        let err = tool_get_session(
            &json!({ "session_id": "claude:test1", "include_refs": true }),
            &config,
            &db,
        )
        .unwrap_err();
        assert!(err.contains("include_refs only applies with message_seq"));

        assert!(tool_get_session(
            &json!({
                "session_id": "claude:test1",
                "context": 0,
                "include_refs": false,
                "preview_chars": config.mcp.preview_chars,
                "response_format": "concise"
            }),
            &config,
            &db,
        )
        .is_ok());
    }

    #[test]
    fn get_session_prefers_concrete_output_selector_names() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let transcript = tool_get_session(
            &json!({ "session_id": "claude:test1", "transcript_lines": -3 }),
            &config,
            &db,
        )
        .unwrap();
        assert!(transcript.contains("- Transcript lines returned: last 3"));

        let window = parse(
            &tool_get_session(
                &json!({
                    "session_id": "claude:test1",
                    "message_seq": 1,
                    "context": 1,
                    "include_refs": true,
                    "preview_chars": 80
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(window["anchor_seq"], 1);
        assert_eq!(window["messages"].as_array().unwrap().len(), 3);

        let err = tool_get_session(
            &json!({
                "session_id": "claude:test1",
                "summary": true,
                "transcript_lines": -3
            }),
            &config,
            &db,
        )
        .unwrap_err();
        assert!(err.contains("Use only one"));
    }

    #[test]
    fn get_session_full_transcript_is_bounded_by_default() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let out = tool_get_session(&json!({ "session_id": "claude:test1" }), &config, &db).unwrap();
        assert!(out.contains("- Transcript lines returned: last 40 (truncated; 0 returns the entire transcript and may be very large)"));
        let transcript = out.structured_content.as_ref().unwrap()["transcript"].clone();
        assert_eq!(transcript["total_lines"], 405);
        assert_eq!(transcript["lines_returned"], 40);
        assert_eq!(transcript["selected_edge"], "tail");
        assert_eq!(transcript["complete"], false);
        assert!(out.contains("transcript line 365"));
        assert!(out.contains("transcript line 404"));
        assert!(
            !out.contains("transcript line 364"),
            "bare get_session should not return the entire transcript by default"
        );

        let full = tool_get_session(
            &json!({ "session_id": "claude:test1", "transcript_lines": 0 }),
            &config,
            &db,
        )
        .unwrap();
        assert!(full.contains("- Transcript lines returned: all"));
        assert!(full.contains("transcript line 404"));

        let tail = tool_get_session(
            &json!({ "session_id": "claude:test1", "transcript_lines": -3 }),
            &config,
            &db,
        )
        .unwrap();
        assert!(tail.contains("- Transcript lines returned: last 3 (truncated; 0 returns the entire transcript and may be very large)"));
        assert!(!tail.contains("transcript line 401"));
        assert!(tail.contains("transcript line 402"));
        assert!(tail.contains("transcript line 404"));
    }

    #[test]
    fn the_mcp_kind_vocabulary_is_the_stored_spelling_so_sql_and_mcp_agree() {
        for value in message_kind_values() {
            // An MCP caller reads kinds here and then writes the same word into
            // query_session_index SQL. That only works while this vocabulary is the stored one,
            // which is what separates it from `aise --kind`, where the CLI hyphenates and the
            // SQL surface refuses the result.
            assert!(
                crate::sql_query::stored_kind_for_typed_spelling(value).is_none(),
                "kinds publishes {value}, which the index does not store"
            );
        }
    }

    #[test]
    fn the_term_frequency_view_is_named_because_nothing_else_on_mcp_reports_a_total() {
        // search_messages pages: it reports has_more, next_offset, and returned, never a total. So
        // "how many messages contain this term" has one answer on MCP, the fts5vocab view, which
        // query_session_index can read but the schema listing deliberately omits as machinery.
        // The CLI answers it with `aise vocab --prefix`; MCP has no such tool, so the description
        // is the only place the view can be disclosed.
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let listed = parse(&tool_query_session_index(&json!({}), &config).unwrap());
        let names = listed["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();
        assert!(
            !names.contains(&"messages_vocab".to_string()),
            "the listing stays curated: {names:?}"
        );

        let description = handle_tools_list(Some(json!(1)), &config)["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "query_session_index")
            .expect("query_session_index tool")["description"]
            .as_str()
            .unwrap()
            .to_string();
        for required in ["messages_vocab", "term", "doc", "cnt"] {
            assert!(
                description.contains(required),
                "query_session_index never names {required}, so a caller counting messages per \
                 term cannot find the only view that reports it: {description}"
            );
        }
    }

    #[test]
    fn query_session_index_lists_schema_and_runs_safe_read_only_sql() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);

        let schema = parse(&tool_query_session_index(&json!({}), &config).unwrap());
        let names = schema["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap_or(""))
            .collect::<Vec<_>>();
        assert!(names.contains(&"sessions"));
        assert!(names.contains(&"messages"));
        assert!(!names.contains(&"messages_fts"));
        assert!(!names.contains(&"messages_fts_data"));

        let columns = parse(
            &tool_query_session_index(&json!({ "schema_table": "messages" }), &config).unwrap(),
        );
        assert!(columns["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "content"));
        // An MCP caller writing SQL reads this table's columns instead of the CLI help, so the
        // provider-spelling disclosure has to travel with the column here too.
        let tool_name = columns["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "tool_name")
            .expect("tool_name column");
        assert!(
            tool_name["note"]
                .as_str()
                .unwrap_or_default()
                .contains("mcp__"),
            "{tool_name}"
        );

        let rows = parse(
            &tool_query_session_index(
                &json!({
                    "sql": "select role, count(*) as n from messages group by role order by role",
                    "limit": 10
                }),
                &config,
            )
            .unwrap(),
        );
        assert_eq!(rows["columns"], json!(["role", "n"]));
        assert!(rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["role"] == "user" && row["n"] == 2));
    }

    #[test]
    fn query_session_index_uses_its_mcp_only_timeout_default() {
        let mut config = Config::default();
        config.db.query_timeout_ms = 1_111;
        config.mcp.query_timeout_ms = 2_222;

        let tool = tool_input_schema(&config, "query_session_index");
        let schema = &tool["inputSchema"];
        assert_eq!(schema["properties"]["timeout_ms"]["default"], 2_222);
        let description = schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .unwrap();
        assert!(description.contains("MCP-only raw-SQL availability guard"));
        assert!(description.contains("independent of native CLI/Rust SQL defaults"));
        assert!(!description.contains("1111"));
    }

    #[test]
    fn query_session_index_rejects_sql_but_keeps_schema_visible_in_allowed_roots_mode() {
        let (dir, _db) = fixture();
        let mut config = config_for_fixture(&dir);
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots = vec![dir.path().to_string_lossy().into_owned()];

        let error = tool_query_session_index(&json!({ "sql": "select * from sessions" }), &config)
            .unwrap_err();
        assert!(error.contains("arbitrary SQL cannot enforce workspace authority"));
        let schema = parse(&tool_query_session_index(&json!({}), &config).unwrap());
        assert!(schema["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "sessions"));
    }

    #[test]
    fn query_session_index_rejects_unsafe_sql_and_truncates_large_cells() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);

        assert!(
            tool_query_session_index(&json!({ "sql": "select 1; select 2" }), &config).is_err()
        );
        let pragma_err =
            tool_query_session_index(&json!({ "sql": "pragma wal_checkpoint" }), &config)
                .unwrap_err();
        assert!(pragma_err.contains("read-only") || pragma_err.contains("SELECT-style"));
        let attach_err = tool_query_session_index(
            &json!({ "sql": "attach database '/tmp/x.db' as x" }),
            &config,
        )
        .unwrap_err();
        assert!(attach_err.contains("read-only") || attach_err.contains("blocked"));
        let mode_err = tool_query_session_index(
            &json!({ "sql": "select 1", "schema_table": "messages" }),
            &config,
        )
        .unwrap_err();
        assert!(mode_err.contains("one mode at a time"));

        let empty_sql_schema =
            parse(&tool_query_session_index(&json!({ "sql": "" }), &config).unwrap());
        assert!(empty_sql_schema["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "messages"));

        let out = parse(
            &tool_query_session_index(
                &json!({
                    "sql": "select content from messages where seq = 1",
                    "max_cell_chars": 8
                }),
                &config,
            )
            .unwrap(),
        );
        assert_eq!(out["next_offset"], Value::Null);
        assert_eq!(out["truncated_cell_char_limit"], 8);
        assert!(out["rows"][0]["content"]
            .as_str()
            .unwrap()
            .ends_with("[truncated]"));
    }

    #[test]
    fn production_mcp_adapter_implements_the_official_rmcp_server_contract() {
        fn assert_official_server<T: rmcp::ServerHandler>() {}

        assert_official_server::<OfficialMcpServer>();
    }

    #[test]
    fn official_rmcp_transport_negotiates_and_serves_the_canonical_tool_catalogue() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let config = config_for_fixture(&dir);
        let index_path = config.db_path();
        assert!(!index_path.exists(), "fixture starts without an index");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");

            let peer_info = client.peer_info().expect("server initialization metadata");
            assert_eq!(peer_info.server_info.name, "ai-session-search");
            assert_eq!(
                peer_info.server_info.title.as_deref(),
                Some("AI Session Search")
            );
            assert_eq!(
                peer_info.instructions.as_deref(),
                Some(crate::integrations::agent_instructions())
            );

            let tools = client.peer().list_tools(None).await.expect("tools/list");
            let names = tools
                .tools
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect::<Vec<_>>();
            assert_eq!(
                names,
                [
                    "search_sessions",
                    "get_session",
                    "list_sessions",
                    "get_resume_command",
                    "search_messages",
                    "run_skill_capability",
                    "get_index_status",
                    "query_session_index",
                ]
            );
            assert!(tools.tools.iter().all(|tool| tool.output_schema.is_some()));
            let search_messages = tools
                .tools
                .iter()
                .find(|tool| tool.name == "search_messages")
                .expect("search_messages is advertised");
            let input_schema = serde_json::to_value(&search_messages.input_schema)
                .expect("rmcp schema serializes");
            // The catalogue used to carry `x-aise-specification` and `x-aise-parameters` so a
            // test could read the registry mapping back off the wire. No client reads either, so
            // they are gone and the transport asserts what a transport owns: that the schema
            // survives rmcp intact and still advertises the field the planner maps to
            // `result_extent`. The mapping itself is asserted against
            // `message_search_mcp_fields` in
            // `search_messages_schema_covers_every_canonical_parameter_and_mcp_property`.
            assert!(
                input_schema["properties"]["limit"]["type"] == json!("integer"),
                "{input_schema}"
            );
            assert!(
                message_search_mcp_fields(MessageSearchParameter::ResultExtent).contains(&"limit"),
                "limit is the wire spelling of the result_extent identity"
            );
            for key in ["x-aise-specification", "x-aise-parameters"] {
                assert!(
                    !serde_json::to_string(&input_schema)
                        .expect("schema serializes")
                        .contains(key),
                    "{key} is back on the wire, where no MCP client reads it"
                );
            }

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn canonical_tool_catalogue_does_not_require_or_create_an_index() {
        let dir = tempfile::tempdir().unwrap();
        let config = config_for_fixture(&dir);
        let index_path = config.db_path();
        assert!(!index_path.exists(), "fixture starts without an index");

        let catalogue = handle_tools_list(None, &config);

        assert_eq!(
            catalogue["result"]["tools"]
                .as_array()
                .expect("tools array")
                .len(),
            8
        );
        assert!(
            !index_path.exists(),
            "catalogue generation must remain available before refresh creates the index"
        );
    }

    #[test]
    fn official_rmcp_initialization_schedules_the_existing_refresh_worker() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let state = Arc::clone(&server.inner);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");

            let mut scheduled = false;
            for _ in 0..50 {
                scheduled = state.lock().unwrap().refresh_worker.handle.is_some();
                if scheduled {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
            drop(state);
            assert!(
                scheduled,
                "official rmcp initialization must reuse the existing refresh worker"
            );
        });
    }

    #[test]
    fn fresh_empty_mcp_search_returns_typed_not_ready_without_waiting_for_the_writer() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let config = config_for_fixture(&dir);
        let app = SessionSearch::open(config.clone()).unwrap();
        drop(app);
        let mut lock = crate::indexer::open_index_update_lock(
            &crate::indexer::index_update_lock_path(&config.db_path()),
        )
        .unwrap();
        let _writer = lock.try_write().unwrap();

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let response = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .expect("not-ready response");

            assert_ne!(response.is_error, Some(true));
            let structured = response.structured_content.expect("typed not-ready status");
            assert_eq!(structured["operation_executed"], false);
            assert!(structured.get("results").is_none());
            assert_eq!(
                structured["index_status"]["snapshot"]["availability"],
                "unavailable"
            );
            assert!(matches!(
                structured["index_status"]["refresh"]["state"].as_str(),
                Some("indexing" | "postponed")
            ));
            assert_eq!(
                structured["next_call"],
                json!({"tool": "get_index_status", "arguments": {}})
            );
            assert!(response.content[0]
                .as_text()
                .is_some_and(|text| text.text.contains("no usable snapshot")));

            let status = client
                .peer()
                .call_tool(rmcp::model::CallToolRequestParams::new("get_index_status"))
                .await
                .expect("status remains callable without a usable snapshot");
            assert_ne!(status.is_error, Some(true));
            let status = status.structured_content.expect("typed index status");
            assert_eq!(
                status["readiness"]["snapshot"]["availability"],
                "unavailable"
            );
            assert!(
                status.get("operation_executed").is_none(),
                "get_index_status executes normally instead of recursively returning not-ready"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_before_query_finishes_initial_preparation_then_returns_executed_empty_results()
    {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::BeforeQuery;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let response = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "not present", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .expect("before-query search response");

            assert_ne!(response.is_error, Some(true));
            let structured = response.structured_content.expect("executed search result");
            assert_eq!(structured["results"], json!([]));
            assert!(
                structured.get("operation_executed").is_none(),
                "an executed empty search must remain distinguishable from not-ready"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_existing_only_missing_index_fails_without_creating_storage() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for_fixture(&dir);
        let missing_parent = dir.path().join("missing");
        let missing_db = missing_parent.join("index.db");
        config.index.db_path = Some(missing_db.to_string_lossy().into_owned());
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let response = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({
                            "query": "not present",
                            "limit": 1,
                            "index_refresh": "existing-only"
                        })
                        .as_object()
                        .unwrap()
                        .clone(),
                    ),
                )
                .await
                .expect("existing-only error response");

            assert_eq!(response.is_error, Some(true));
            let error = response.content[0]
                .as_text()
                .expect("actionable text error")
                .text
                .as_str();
            assert!(error.contains("does not exist"), "{error}");
            assert!(error.contains("aise reindex --full"), "{error}");
            assert!(!missing_db.exists());
            assert!(!missing_parent.exists());

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    #[allow(deprecated)]
    fn official_rmcp_initialization_uses_typed_client_roots_for_restricted_scope() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots.clear();
        config.search.scope.include_invocation_directory = false;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (client_handler, responses) = official_roots_client(true);
            responses
                .send(rmcp::model::ListRootsResult::new(vec![
                    rmcp::model::Root::new(
                        url::Url::from_directory_path(&allowed).unwrap().to_string(),
                    ),
                ]))
                .unwrap();
            let requests = Arc::clone(&client_handler.request_count);
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let state = Arc::clone(&server.inner);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = client_handler
                .serve(client_transport)
                .await
                .expect("rmcp client initializes");

            for _ in 0..50 {
                if requests.load(Ordering::Acquire) == 1
                    && state.lock().unwrap().harness_roots == [allowed.clone()]
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(requests.load(Ordering::Acquire), 1);
            assert_eq!(state.lock().unwrap().harness_roots, [allowed]);

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_without_roots_capability_does_not_request_roots() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots.clear();
        config.search.scope.include_invocation_directory = false;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (client_handler, _responses) = official_roots_client(false);
            let requests = Arc::clone(&client_handler.request_count);
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = client_handler
                .serve(client_transport)
                .await
                .expect("rmcp client initializes");

            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(requests.load(Ordering::Acquire), 0);

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    #[allow(deprecated)]
    fn official_rmcp_root_changes_revoke_immediately_and_discard_stale_responses() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let replacement = dir.path().join("replacement");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&replacement).unwrap();
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots.clear();
        config.search.scope.include_invocation_directory = false;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (client_handler, responses) = official_roots_client(true);
            responses
                .send(rmcp::model::ListRootsResult::new(vec![
                    rmcp::model::Root::new(
                        url::Url::from_directory_path(&allowed).unwrap().to_string(),
                    ),
                ]))
                .unwrap();
            let requests = Arc::clone(&client_handler.request_count);
            let maximum_active = Arc::clone(&client_handler.maximum_active_requests);
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let state = Arc::clone(&server.inner);
            let root_generations = Arc::clone(&server.roots_refresh);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = client_handler
                .serve(client_transport)
                .await
                .expect("rmcp client initializes");
            for _ in 0..50 {
                if state.lock().unwrap().harness_roots == [allowed.clone()] {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                state.lock().unwrap().harness_roots.as_slice(),
                std::slice::from_ref(&allowed)
            );

            client
                .peer()
                .notify_roots_list_changed()
                .await
                .unwrap();
            for _ in 0..50 {
                if requests.load(Ordering::Acquire) == 2
                    && state.lock().unwrap().harness_roots.is_empty()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(
                state.lock().unwrap().harness_roots.is_empty(),
                "authority must be revoked before replacement roots arrive"
            );

            for _ in 0..8 {
                let previous_generation = Arc::clone(
                    &root_generations.lock().unwrap().current_generation,
                );
                client
                    .peer()
                    .notify_roots_list_changed()
                    .await
                    .unwrap();
                for _ in 0..50 {
                    if !Arc::ptr_eq(
                        &root_generations.lock().unwrap().current_generation,
                        &previous_generation,
                    ) {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                assert!(
                    !Arc::ptr_eq(
                        &root_generations.lock().unwrap().current_generation,
                        &previous_generation,
                    ),
                    "each sent change must be processed before the stale response is released"
                );
            }
            responses
                .send(rmcp::model::ListRootsResult::new(vec![
                    rmcp::model::Root::new(
                        url::Url::from_directory_path(&allowed).unwrap().to_string(),
                    ),
                ]))
                .unwrap();
            for _ in 0..50 {
                if requests.load(Ordering::Acquire) == 3 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                requests.load(Ordering::Acquire),
                3,
                "a notification burst must coalesce behind one stale request and one current request"
            );
            assert!(
                state.lock().unwrap().harness_roots.is_empty(),
                "the stale response must not restore revoked roots"
            );

            responses
                .send(rmcp::model::ListRootsResult::new(vec![
                    rmcp::model::Root::new(
                        url::Url::from_directory_path(&replacement)
                            .unwrap()
                            .to_string(),
                    ),
                ]))
                .unwrap();
            for _ in 0..50 {
                if state.lock().unwrap().harness_roots == [replacement.clone()] {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(state.lock().unwrap().harness_roots, [replacement]);
            assert_eq!(
                maximum_active.load(Ordering::Acquire),
                1,
                "root changes must never create overlapping roots/list requests"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    #[allow(deprecated)]
    fn official_rmcp_received_root_change_revokes_without_list_changed_advertisement() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots.clear();
        config.search.scope.include_invocation_directory = false;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (client_handler, responses) =
                official_roots_client_with_capabilities(true, false);
            responses
                .send(rmcp::model::ListRootsResult::new(vec![
                    rmcp::model::Root::new(
                        url::Url::from_directory_path(&allowed).unwrap().to_string(),
                    ),
                ]))
                .unwrap();
            let requests = Arc::clone(&client_handler.request_count);
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let state = Arc::clone(&server.inner);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = client_handler
                .serve(client_transport)
                .await
                .expect("rmcp client initializes");
            for _ in 0..50 {
                if state.lock().unwrap().harness_roots == [allowed.clone()] {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }

            client
                .peer()
                .notify_roots_list_changed()
                .await
                .unwrap();
            for _ in 0..50 {
                if requests.load(Ordering::Acquire) == 2
                    && state.lock().unwrap().harness_roots.is_empty()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(requests.load(Ordering::Acquire), 2);
            assert!(
                state.lock().unwrap().harness_roots.is_empty(),
                "an actually received change must revoke stale roots even when listChanged was not advertised"
            );

            responses
                .send(rmcp::model::ListRootsResult::new(vec![
                    rmcp::model::Root::new("https://example.com/not-local"),
                ]))
                .unwrap();
            for _ in 0..50 {
                if state.lock().unwrap().roots_error.is_some() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let error = state.lock().unwrap().roots_error.clone().unwrap();
            assert!(error.contains("roots[0].uri must use the file scheme"), "{error}");
            assert!(error.contains("local file:// directory URIs"), "{error}");
            assert!(
                error.contains("notifications/roots/list_changed"),
                "{error}"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_transport_shutdown_cancels_a_pending_roots_request() {
        use rmcp::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots.clear();
        config.search.scope.include_invocation_directory = false;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (client_handler, _responses) = official_roots_client(true);
            let requests = Arc::clone(&client_handler.request_count);
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = client_handler
                .serve(client_transport)
                .await
                .expect("rmcp client initializes");
            for _ in 0..50 {
                if requests.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(requests.load(Ordering::Acquire), 1);

            // rmcp 2.2.0 deliberately allows two seconds to drain cancelled handlers. The outer
            // second is test scheduling margin, not a production timeout owned by this project.
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                client.cancel().await.expect("client shutdown");
                server_task.await.unwrap().expect("server shutdown");
            })
            .await
            .expect("transport shutdown must finish within rmcp's cancellation drain");
        });
    }

    #[test]
    fn official_rmcp_existing_only_initialization_does_not_start_a_refresh_writer() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config).unwrap();
            let state = Arc::clone(&server.inner);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");

            tokio::task::yield_now().await;
            assert!(
                state.lock().unwrap().refresh_worker.handle.is_none(),
                "existing-only initialization must not acquire or queue a refresh writer"
            );
            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_completed_tool_call_schedules_refresh_after_reader_cleanup() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(1).unwrap())
                    .unwrap();
            let permits = Arc::clone(&server.reader_permits);
            let observed_available_permits = Arc::new(Mutex::new(Vec::new()));
            let runner_observations = Arc::clone(&observed_available_permits);
            let runner_permits = Arc::clone(&permits);
            let server = server.with_refresh_runner(Arc::new(move |_, _| {
                runner_observations
                    .lock()
                    .unwrap()
                    .push(runner_permits.available_permits());
            }));
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            for _ in 0..50 {
                if observed_available_permits.lock().unwrap().len() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                observed_available_permits.lock().unwrap().as_slice(),
                [1],
                "initialization schedules the first refresh without holding a reader permit"
            );

            let response = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .expect("tool call completes");
            let structured = response
                .structured_content
                .expect("normal search response has structured content");
            assert!(structured.get("results").is_some());
            assert!(
                structured.get("operation_executed").is_none(),
                "successful calls retain the canonical response without readiness metadata"
            );
            for _ in 0..50 {
                if observed_available_permits.lock().unwrap().len() == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                observed_available_permits.lock().unwrap().as_slice(),
                [1, 1],
                "a completed call must enqueue one refresh only after its reader permit is released"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_waits_for_successful_response_delivery_before_scheduling_refresh() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let refresh_runs = Arc::new(AtomicUsize::new(0));
        let runner_runs = Arc::clone(&refresh_runs);
        let gate_enabled = Arc::new(AtomicBool::new(false));
        let response_attempted = Arc::new(tokio::sync::Semaphore::new(0));
        let release_response = Arc::new(tokio::sync::Semaphore::new(0));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config)
                .unwrap()
                .with_refresh_runner(Arc::new(move |_, _| {
                    runner_runs.fetch_add(1, Ordering::AcqRel);
                }));
            let server_transport = rmcp::transport::IntoTransport::<
                rmcp::RoleServer,
                std::io::Error,
                rmcp::transport::async_rw::TransportAdapterAsyncCombinedRW,
            >::into_transport(server_transport);
            let server_transport = GatedServerTransport {
                inner: server_transport,
                enabled: Arc::clone(&gate_enabled),
                response_attempted: Arc::clone(&response_attempted),
                release_response: Arc::clone(&release_response),
            };
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            for _ in 0..50 {
                if refresh_runs.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(refresh_runs.load(Ordering::Acquire), 1);

            gate_enabled.store(true, Ordering::Release);
            let peer = client.peer().clone();
            let call = tokio::spawn(async move {
                peer.call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
            });
            let attempted = response_attempted
                .acquire()
                .await
                .expect("response send reaches the gate");
            attempted.forget();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(
                refresh_runs.load(Ordering::Acquire),
                1,
                "refresh must not start while the tool response remains undelivered"
            );

            release_response.add_permits(1);
            call.await.unwrap().expect("tool response is delivered");
            for _ in 0..50 {
                if refresh_runs.load(Ordering::Acquire) == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                refresh_runs.load(Ordering::Acquire),
                2,
                "successful response delivery starts exactly one post-call refresh"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_failed_response_delivery_releases_admission_without_refreshing() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let refresh_runs = Arc::new(AtomicUsize::new(0));
        let runner_runs = Arc::clone(&refresh_runs);
        let fail_enabled = Arc::new(AtomicBool::new(false));
        let response_attempted = Arc::new(tokio::sync::Semaphore::new(0));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(1).unwrap())
                    .unwrap()
                    .with_refresh_runner(Arc::new(move |_, _| {
                        runner_runs.fetch_add(1, Ordering::AcqRel);
                    }));
            let reader_permits = Arc::clone(&server.reader_permits);
            let server_transport = rmcp::transport::IntoTransport::<
                rmcp::RoleServer,
                std::io::Error,
                rmcp::transport::async_rw::TransportAdapterAsyncCombinedRW,
            >::into_transport(server_transport);
            let server_transport = FailingResponseTransport {
                inner: server_transport,
                enabled: Arc::clone(&fail_enabled),
                response_attempted: Arc::clone(&response_attempted),
            };
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            for _ in 0..50 {
                if refresh_runs.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(refresh_runs.load(Ordering::Acquire), 1);

            fail_enabled.store(true, Ordering::Release);
            let peer = client.peer().clone();
            let call = tokio::spawn(async move {
                peer.call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
            });
            let attempted = response_attempted
                .acquire()
                .await
                .expect("response send reaches the failing transport");
            attempted.forget();
            for _ in 0..50 {
                if reader_permits.available_permits() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                reader_permits.available_permits(),
                1,
                "failed response delivery must release the retained reader permit"
            );
            assert_eq!(
                refresh_runs.load(Ordering::Acquire),
                1,
                "failed response delivery must not start a post-call refresh"
            );

            call.abort();
            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_cancels_an_undelivered_response_without_refreshing() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let refresh_runs = Arc::new(AtomicUsize::new(0));
        let runner_runs = Arc::clone(&refresh_runs);
        let gate_enabled = Arc::new(AtomicBool::new(false));
        let response_attempted = Arc::new(tokio::sync::Semaphore::new(0));
        let release_response = Arc::new(tokio::sync::Semaphore::new(0));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(1).unwrap())
                    .unwrap()
                    .with_refresh_runner(Arc::new(move |_, _| {
                        runner_runs.fetch_add(1, Ordering::AcqRel);
                    }));
            let reader_permits = Arc::clone(&server.reader_permits);
            let server_transport = rmcp::transport::IntoTransport::<
                rmcp::RoleServer,
                std::io::Error,
                rmcp::transport::async_rw::TransportAdapterAsyncCombinedRW,
            >::into_transport(server_transport);
            let server_transport = GatedServerTransport {
                inner: server_transport,
                enabled: Arc::clone(&gate_enabled),
                response_attempted: Arc::clone(&response_attempted),
                release_response: Arc::clone(&release_response),
            };
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            for _ in 0..50 {
                if refresh_runs.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(refresh_runs.load(Ordering::Acquire), 1);

            gate_enabled.store(true, Ordering::Release);
            let request = rmcp::model::ClientRequest::CallToolRequest(rmcp::model::Request::new(
                rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                    json!({"query": "hello", "limit": 1})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            ));
            let call = client
                .send_cancellable_request(request, rmcp::service::PeerRequestOptions::no_options())
                .await
                .expect("tool request is accepted");
            let attempted = response_attempted
                .acquire()
                .await
                .expect("response send reaches the gate");
            attempted.forget();
            assert_eq!(
                reader_permits.available_permits(),
                0,
                "an undelivered eligible response retains its bounded admission permit"
            );

            call.cancel(Some("cancel undelivered response".to_string()))
                .await
                .expect("cancellation notification is sent");
            for _ in 0..50 {
                if reader_permits.available_permits() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                reader_permits.available_permits(),
                1,
                "cancelling an undelivered response must release its retained permit"
            );
            release_response.add_permits(1);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(
                refresh_runs.load(Ordering::Acquire),
                1,
                "a cancelled response must not schedule a refresh if its stale send later succeeds"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_direct_existing_only_and_invalid_calls_do_not_schedule_refresh() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let refresh_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner_runs = Arc::clone(&refresh_runs);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config)
                .unwrap()
                .with_refresh_runner(Arc::new(move |_, _| {
                    runner_runs.fetch_add(1, Ordering::AcqRel);
                }));
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            for _ in 0..50 {
                if refresh_runs.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(refresh_runs.load(Ordering::Acquire), 1);

            for request in [
                rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                    json!({"query": "hello", "limit": 1, "index_refresh": "existing-only"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                rmcp::model::CallToolRequestParams::new("query_session_index"),
                rmcp::model::CallToolRequestParams::new("not_a_tool"),
            ] {
                client.peer().call_tool(request).await.unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            assert_eq!(
                refresh_runs.load(Ordering::Acquire),
                1,
                "direct schema, existing-only, and invalid calls must preserve their non-refreshing lifecycle"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn refresh_worker_coalesces_a_burst_into_one_trailing_reconciliation() {
        let config = Config::default();
        let (run_started, run_started_rx) = mpsc::channel();
        let (release_first, release_first_rx) = mpsc::sync_channel(1);
        let release_first_rx = Arc::new(Mutex::new(release_first_rx));
        let runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner_runs = Arc::clone(&runs);
        let runner_release = Arc::clone(&release_first_rx);
        let mut worker = RefreshWorker::default();
        worker.runner = Arc::new(move |_, _| {
            let run = runner_runs.fetch_add(1, Ordering::AcqRel) + 1;
            run_started.send(run).unwrap();
            if run == 1 {
                runner_release.lock().unwrap().recv().unwrap();
            }
        });

        worker.schedule(config.clone());
        assert_eq!(
            run_started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            1
        );
        for _ in 0..10 {
            worker.schedule(config.clone());
        }
        release_first.send(()).unwrap();

        assert_eq!(
            run_started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("one trigger arriving during the active run must be retained"),
            2
        );
        assert!(
            run_started_rx
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "a burst must not create more than one trailing reconciliation"
        );
        drop(worker);
        assert_eq!(runs.load(Ordering::Acquire), 2);
    }

    #[test]
    fn long_lived_official_rmcp_indexes_appended_replaced_and_new_sessions_after_refresh_boundaries(
    ) {
        use rmcp::ServiceExt as _;
        use std::io::Write as _;

        async fn wait_for_refresh(
            receiver: Arc<Mutex<mpsc::Receiver<()>>>,
            description: &'static str,
        ) {
            tokio::task::spawn_blocking(move || {
                receiver
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap_or_else(|_| panic!("{description}"));
            })
            .await
            .unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        let sources = dir.path().join("claude");
        std::fs::create_dir_all(&sources).unwrap();
        let transcript = sources.join("freshness-test.jsonl");
        std::fs::write(
            &transcript,
            concat!(
                r#"{"sessionId":"freshness-test","type":"user","message":{"role":"user","content":"initial marker"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").display().to_string());
        config.index.auto_reindex_interval_ms = 0;
        let empty_sources = dir.path().join("empty");
        std::fs::create_dir_all(&empty_sources).unwrap();
        let empty_sources = vec![empty_sources.display().to_string()];
        for provider in [
            &mut config.providers.claude_desktop,
            &mut config.providers.codex,
            &mut config.providers.cursor,
            &mut config.providers.antigravity,
            &mut config.providers.pi,
            &mut config.providers.aistudio,
            &mut config.providers.gemini_cli,
        ] {
            provider.paths = empty_sources.clone();
        }
        config.providers.claude.paths = vec![sources.display().to_string()];
        let app = SessionSearch::open(config.clone()).unwrap();
        assert_eq!(app.index().reindex(false).unwrap(), (1, 1));
        drop(app);

        let (refresh_finished, refresh_finished_rx) = mpsc::channel();
        let refresh_finished_rx = Arc::new(Mutex::new(refresh_finished_rx));
        let refresh_runner = Arc::new(move |config: &Config, cancel: &AtomicBool| {
            run_background_refresh(config, cancel);
            refresh_finished.send(()).unwrap();
        });
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config)
                .unwrap()
                .with_refresh_runner(refresh_runner);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            wait_for_refresh(
                Arc::clone(&refresh_finished_rx),
                "initialization refresh did not finish",
            )
            .await;

            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&transcript)
                .unwrap();
            writeln!(
                file,
                r#"{{"sessionId":"freshness-test","type":"assistant","message":{{"role":"assistant","content":"post start freshness marker"}}}}"#
            )
            .unwrap();
            file.flush().unwrap();
            let first = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "post start freshness marker", "limit": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                first.structured_content.as_ref().unwrap()["results"]
                    .as_array()
                    .map(Vec::len),
                Some(0),
                "the response generation is fixed before its post-call refresh"
            );

            wait_for_refresh(
                Arc::clone(&refresh_finished_rx),
                "append refresh did not finish",
            )
            .await;
            let second = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "post start freshness marker", "limit": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                second.structured_content.as_ref().unwrap()["results"]
                    .as_array()
                    .map(Vec::len),
                Some(1)
            );
            wait_for_refresh(
                Arc::clone(&refresh_finished_rx),
                "post-append verification refresh did not finish",
            )
            .await;

            std::fs::write(
                sources.join("new-session.jsonl"),
                concat!(
                    r#"{"sessionId":"new-session","type":"user","message":{"role":"user","content":"newly discovered session marker"}}"#,
                    "\n",
                ),
            )
            .unwrap();
            let before_new_session_refresh = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "newly discovered session marker", "limit": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                before_new_session_refresh.structured_content.as_ref().unwrap()["results"]
                    .as_array()
                    .map(Vec::len),
                Some(0)
            );
            wait_for_refresh(
                Arc::clone(&refresh_finished_rx),
                "new-session refresh did not finish",
            )
            .await;
            let after_new_session_refresh = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "newly discovered session marker", "limit": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                after_new_session_refresh.structured_content.as_ref().unwrap()["results"]
                    .as_array()
                    .map(Vec::len),
                Some(1)
            );
            wait_for_refresh(
                Arc::clone(&refresh_finished_rx),
                "post-new-session verification refresh did not finish",
            )
            .await;

            std::fs::write(
                &transcript,
                concat!(
                    r#"{"sessionId":"freshness-test","type":"user","message":{"role":"user","content":"replacement session marker"}}"#,
                    "\n",
                ),
            )
            .unwrap();
            let before_replacement_refresh = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "replacement session marker", "limit": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                before_replacement_refresh.structured_content.as_ref().unwrap()["results"]
                    .as_array()
                    .map(Vec::len),
                Some(0)
            );
            wait_for_refresh(
                Arc::clone(&refresh_finished_rx),
                "replacement refresh did not finish",
            )
            .await;
            let after_replacement_refresh = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "replacement session marker", "limit": 5})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                after_replacement_refresh.structured_content.as_ref().unwrap()["results"]
                    .as_array()
                    .map(Vec::len),
                Some(1)
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_transport_returns_canonical_results_and_actionable_tool_errors() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server_task = tokio::spawn(async move {
                OfficialMcpServer::new(config)
                    .unwrap()
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");

            let search = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .expect("search_messages routes");
            assert_eq!(search.is_error, Some(false));
            assert_eq!(
                search.structured_content.as_ref().unwrap()["page"]["returned"],
                1
            );
            assert!(search.content[0]
                .as_text()
                .unwrap()
                .text
                .contains("structuredContent is the authoritative response"));

            let invalid = client
                .peer()
                .call_tool(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "field_view": {"kind": "max_chars", "max_chars": 0}})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                )
                .await
                .expect("invalid tool input remains a caller-visible tool result");
            assert_eq!(invalid.is_error, Some(true));
            let invalid_text = &invalid.content[0].as_text().unwrap().text;
            assert!(invalid_text.contains("field_view"), "{invalid_text}");
            assert!(invalid_text.contains("schema alternative"), "{invalid_text}");

            let unknown = client
                .peer()
                .call_tool(rmcp::model::CallToolRequestParams::new("search_message"))
                .await
                .expect("unknown tool remains a caller-visible tool result");
            assert_eq!(unknown.is_error, Some(true));
            let unknown_text = &unknown.content[0].as_text().unwrap().text;
            assert!(
                unknown_text.contains("did you mean \"search_messages\"?"),
                "{unknown_text}"
            );
            assert!(unknown_text.contains("\"get_session\""), "{unknown_text}");

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_same_server_runs_two_database_readers_simultaneously() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let probe = Arc::new(TestReaderProbe::new(2));
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(2).unwrap())
                    .unwrap()
                    .with_reader_probe(probe.clone());
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let first_peer = client.peer().clone();
            let second_peer = client.peer().clone();
            let first = tokio::spawn(async move {
                first_peer
                    .call_tool(
                        rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                            json!({"query": "hello", "limit": 1})
                                .as_object()
                                .unwrap()
                                .clone(),
                        ),
                    )
                    .await
            });
            let second = tokio::spawn(async move {
                second_peer
                    .call_tool(
                        rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                            json!({"query": "hello", "limit": 1})
                                .as_object()
                                .unwrap()
                                .clone(),
                        ),
                    )
                    .await
            });

            let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                (first.await.unwrap(), second.await.unwrap())
            })
            .await
            .expect("two readers must reach the rendezvous instead of serializing");
            assert_eq!(first.unwrap().is_error, Some(false));
            assert_eq!(second.unwrap().is_error, Some(false));
            assert_eq!(probe.max_active.load(Ordering::Acquire), 2);

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_cancellation_removes_a_call_waiting_for_reader_admission() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let probe = Arc::new(TestReaderProbe::block_first_until_test());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(1).unwrap())
                    .unwrap()
                    .with_reader_probe(Arc::clone(&probe));
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let request = || {
                rmcp::model::ClientRequest::CallToolRequest(rmcp::model::Request::new(
                    rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                        json!({"query": "hello", "limit": 1})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                ))
            };

            let first = client
                .send_cancellable_request(
                    request(),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
                .unwrap();
            for _ in 0..50 {
                if probe.entries.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(probe.entries.load(Ordering::Acquire), 1);

            let waiting = client
                .send_cancellable_request(
                    request(),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
                .unwrap();
            for _ in 0..50 {
                if probe.admission_attempts.load(Ordering::Acquire) == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let queued_before_cancel = probe.admission_attempts.load(Ordering::Acquire) == 2;
            waiting
                .cancel(Some("test cancels queued search".to_string()))
                .await
                .unwrap();
            for _ in 0..50 {
                if probe.cancelled_admissions.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let cancelled_before_release =
                probe.cancelled_admissions.load(Ordering::Acquire) == 1;
            let release_probe = Arc::clone(&probe);
            tokio::task::spawn_blocking(move || release_probe.release_first())
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), first.await_response())
                .await
                .expect("admitted search finishes")
                .expect("admitted search returns a response");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                queued_before_cancel,
                "the second request must reach reader admission before the test cancels it"
            );
            assert!(
                cancelled_before_release,
                "the queued request must observe cancellation before the admitted reader releases its permit"
            );
            assert_eq!(
                probe.entries.load(Ordering::Acquire),
                1,
                "a cancelled admission waiter must never open a reader"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn dropping_an_official_request_guard_cancels_its_blocking_work() {
        let cancellation = Arc::new(QueryCancellation::new());
        {
            let _guard = CancelBlockingWorkOnDrop::new(Arc::clone(&cancellation));
            assert!(!cancellation.is_cancelled());
        }
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn official_rmcp_cancellation_stops_an_active_reader_before_dispatch() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let probe = Arc::new(TestReaderProbe::block_first_until_test());
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(1).unwrap())
                    .unwrap()
                    .with_reader_probe(Arc::clone(&probe));
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let handle = client
                .send_cancellable_request(
                    rmcp::model::ClientRequest::CallToolRequest(rmcp::model::Request::new(
                        rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                            json!({"query": "hello", "limit": 1})
                                .as_object()
                                .unwrap()
                                .clone(),
                        ),
                    )),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
                .unwrap();
            for _ in 0..50 {
                if probe.entries.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(probe.entries.load(Ordering::Acquire), 1);

            handle
                .cancel(Some("test cancels active search".to_string()))
                .await
                .unwrap();
            for _ in 0..50 {
                if probe.cancelled_active.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let cancelled_before_release = probe.cancelled_active.load(Ordering::Acquire) == 1;
            let release_probe = Arc::clone(&probe);
            tokio::task::spawn_blocking(move || release_probe.release_first())
                .await
                .unwrap();
            for _ in 0..50 {
                if probe.active.load(Ordering::Acquire) == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(probe.active.load(Ordering::Acquire), 0);
            assert!(
                cancelled_before_release,
                "the active request must observe cancellation before semantic dispatch resumes"
            );
            assert_eq!(
                probe.dispatches.load(Ordering::Acquire),
                0,
                "a cancelled active reader must stop before semantic dispatch"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_raw_sql_cancellation_preempts_its_longer_timeout() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::with_reader_bound(
                config,
                NonZeroUsize::new(1).unwrap(),
            )
            .unwrap();
            let permits = Arc::clone(&server.reader_permits);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            let aliases = (0..18)
                .map(|index| format!("messages m{index}"))
                .collect::<Vec<_>>()
                .join(" cross join ");
            let terms = (0..18)
                .map(|index| format!("length(m{index}.content)"))
                .collect::<Vec<_>>()
                .join(" + ");
            let long_sql = format!("select sum({terms}) from {aliases}");
            let handle = client
                .send_cancellable_request(
                    rmcp::model::ClientRequest::CallToolRequest(rmcp::model::Request::new(
                        rmcp::model::CallToolRequestParams::new("query_session_index")
                            .with_arguments(
                                json!({
                                    "sql": long_sql,
                                    "timeout_ms": 1000
                                })
                                .as_object()
                                .unwrap()
                                .clone(),
                            ),
                    )),
                    rmcp::service::PeerRequestOptions::no_options(),
                )
                .await
                .unwrap();
            for _ in 0..50 {
                if permits.available_permits() == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(permits.available_permits(), 0);

            let cancelled_at = std::time::Instant::now();
            handle
                .cancel(Some("test cancels raw SQL".to_string()))
                .await
                .unwrap();
            for _ in 0..200 {
                if permits.available_permits() == 1 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let cancellation_latency = cancelled_at.elapsed();
            assert_eq!(permits.available_permits(), 1);
            assert!(
                cancellation_latency < std::time::Duration::from_millis(500),
                "request cancellation took {cancellation_latency:?}, so the 1000 ms SQL timeout won instead of cancellation"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn official_rmcp_readers_serve_one_wal_generation_while_the_refresh_writer_commits() {
        use rmcp::ServiceExt as _;

        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let db_path = config.db_path();
        let (writer_started, writer_started_rx) = mpsc::sync_channel(1);
        let (release_writer, release_writer_rx) = mpsc::sync_channel(1);
        let (writer_finished, writer_finished_rx) = mpsc::sync_channel(1);
        let release_writer_rx = Arc::new(Mutex::new(release_writer_rx));
        let refresh_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let refresh_runner = Arc::new(move |config: &Config, _cancel: &AtomicBool| {
            if refresh_runs.fetch_add(1, Ordering::AcqRel) > 0 {
                return;
            }
            crate::indexer::with_index_update_lock(config, || {
                let connection = rusqlite::Connection::open(&db_path)?;
                connection.execute_batch("begin immediate")?;
                connection.execute(
                    "update messages set content = 'alpha hello refreshed'
                     where session_id = 'claude:test1' and seq = 0",
                    [],
                )?;
                writer_started.send(()).unwrap();
                release_writer_rx.lock().unwrap().recv().unwrap();
                connection.execute_batch("commit")?;
                writer_finished.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        });

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server =
                OfficialMcpServer::with_reader_bound(config, NonZeroUsize::new(2).unwrap())
                    .unwrap()
                    .with_refresh_runner(refresh_runner);
            let server_task = tokio::spawn(async move {
                server
                    .serve_transport(server_transport)
                    .await
                    .expect("official rmcp server initializes")
                    .waiting()
                    .await
            });
            let client = ().serve(client_transport).await.expect("rmcp client initializes");
            tokio::task::spawn_blocking(move || writer_started_rx.recv().unwrap())
                .await
                .unwrap();

            let request = || {
                rmcp::model::CallToolRequestParams::new("search_messages").with_arguments(
                    json!({"query": "alpha hello", "limit": 1})
                        .as_object()
                        .unwrap()
                        .clone(),
                )
            };
            let first_peer = client.peer().clone();
            let second_peer = client.peer().clone();
            let first = tokio::spawn(async move { first_peer.call_tool(request()).await });
            let second = tokio::spawn(async move { second_peer.call_tool(request()).await });
            let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                (
                    first.await.unwrap().unwrap(),
                    second.await.unwrap().unwrap(),
                )
            })
            .await
            .expect("WAL readers must not wait for the refresh writer");

            for response in [&first, &second] {
                assert_eq!(
                    response.structured_content.as_ref().unwrap()["results"][0]["presentation"]
                        ["field_view"]["text"],
                    "alpha hello there"
                );
            }

            release_writer.send(()).unwrap();
            tokio::task::spawn_blocking(move || writer_finished_rx.recv().unwrap())
                .await
                .unwrap();
            let refreshed = client.peer().call_tool(request()).await.unwrap();
            assert_eq!(
                refreshed.structured_content.as_ref().unwrap()["results"][0]["presentation"]
                    ["field_view"]["text"],
                "alpha hello refreshed"
            );

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    /// `raw_metadata_json` is the provider's verbatim metadata blob and is unbounded: codex
    /// embeds its whole sandbox policy, ~2-3 KB per session. Measured over a 30-session
    /// listing it was 24,929 of 56,667 characters (44%), and `list_sessions(limit=30)` failed
    /// outright with "result (55,824 characters) exceeds maximum allowed tokens" while the
    /// session-level tools offered no way to ask for less. It is omitted by default and
    /// restored only on request.
    #[test]
    fn session_tools_omit_raw_metadata_unless_included() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        // A codex-shaped blob: the real payload is the escaped sandbox policy.
        let raw = json!({"sandbox_policy": "x".repeat(2048)}).to_string();
        let mut parsed = minimal_record(Provider::Codex, Path::new("/raw.jsonl"), String::new());
        parsed.session.id = "codex:rawmeta".to_string();
        parsed.session.provider_session_id = "rawmeta".to_string();
        parsed.session.title = Some("Sandboxed".to_string());
        parsed.session.raw_metadata_json = Some(raw);
        db.upsert_session(&parsed, 0, 0).unwrap();

        let find = |response: &Value| -> Value {
            response["result"]["structuredContent"]["sessions"]
                .as_array()
                .expect("sessions array")
                .iter()
                .find(|s| s["id"] == "codex:rawmeta")
                .expect("the raw-metadata session must be returned")
                .clone()
        };

        for (tool, args) in [
            ("list_sessions", json!({})),
            ("search_sessions", json!({ "query": "Sandboxed" })),
        ] {
            let session = find(&call_tool(tool, args.clone(), &config, &db));
            assert!(
                !session
                    .as_object()
                    .unwrap()
                    .contains_key("raw_metadata_json"),
                "{tool} must omit raw_metadata_json by default; it is what exceeded the token cap"
            );
            assert_eq!(
                session["provider_session_id"], "rawmeta",
                "{tool} must still return the rest of the record"
            );

            let mut opted_in = args;
            opted_in["include"] = json!([INCLUDE_RAW_METADATA]);
            let session = find(&call_tool(tool, opted_in, &config, &db));
            assert!(
                session["raw_metadata_json"]
                    .as_str()
                    .is_some_and(|text| text.contains("sandbox_policy")),
                "{tool} must return raw_metadata_json when include=[\"{INCLUDE_RAW_METADATA}\"]"
            );
        }
    }

    /// Before this, the session-level tools had no payload control at all while
    /// search_messages already had response_format/preview_chars/lines_per_message, so a caller
    /// who hit the response limit had nothing to turn down. Omitting it must leave text
    /// complete, so no existing caller is silently truncated.
    #[test]
    fn session_tools_bound_free_text_only_when_preview_chars_is_given() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let long = "z".repeat(500);
        let mut parsed = minimal_record(Provider::Codex, Path::new("/long.jsonl"), String::new());
        parsed.session.id = "codex:longtext".to_string();
        parsed.session.provider_session_id = "longtext".to_string();
        parsed.session.title = Some(format!("Verbose {long}"));
        parsed.session.summary = Some(long.clone());
        parsed.session.preview_text = long;
        db.upsert_session(&parsed, 0, 0).unwrap();

        let find = |response: &Value| -> Value {
            response["result"]["structuredContent"]["sessions"]
                .as_array()
                .expect("sessions array")
                .iter()
                .find(|s| s["id"] == "codex:longtext")
                .expect("the long-text session must be returned")
                .clone()
        };

        for (tool, args) in [
            ("list_sessions", json!({})),
            ("search_sessions", json!({ "query": "Verbose" })),
        ] {
            let complete = find(&call_tool(tool, args.clone(), &config, &db));
            assert!(
                complete["preview_text"].as_str().unwrap().len() >= 500,
                "{tool} must return complete text when preview_chars is omitted"
            );

            let mut bounded = args;
            bounded["preview_chars"] = json!(40);
            let bounded = find(&call_tool(tool, bounded, &config, &db));
            for field in ["title", "summary", "preview_text"] {
                let text = bounded[field].as_str().unwrap_or_default();
                assert!(
                    text.chars().count() <= 40,
                    "{tool} must bound {field} to preview_chars, got {} chars",
                    text.chars().count()
                );
            }
            // Identity fields are never truncated: a cut id or path is unusable.
            assert_eq!(bounded["id"], "codex:longtext");
            assert_eq!(bounded["provider_session_id"], "longtext");
        }

        // A supplied value is still validated; 0 is rejected rather than meaning "complete".
        let rejected = call_tool("list_sessions", json!({ "preview_chars": 0 }), &config, &db);
        assert_eq!(rejected["result"]["isError"], true);
        assert!(rejected["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("preview_chars must be 1 through")));
    }

    #[test]
    fn get_index_status_returns_shared_parser_health_and_repairs() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool("get_index_status", json!({}), &config, &db);
        let status = &response["result"]["structuredContent"];
        assert_eq!(
            status["parser_health"]["expected_schema_version"],
            crate::db::SCHEMA_VERSION
        );
        assert!(status["parser_health"]["providers"].is_array());
        assert!(status["repairable_stale_sessions"].is_number());
        assert!(status["unavailable_stale_sessions"].is_number());
        let provider = &status["providers"][0];
        assert!(provider["cli_available"].is_boolean());
        assert!(provider["roots"].is_array());
        assert!(provider["discovered_files"].is_number());
        assert!(provider["indexed_sessions"].is_number());
        assert!(provider["repairable_stale_sessions"].is_number());
        assert!(provider["unavailable_stale_sessions"].is_number());
        assert!(provider["resume_command"].is_null() || provider["resume_command"].is_string());
        assert_eq!(status["repair_commands"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn every_advertised_tool_declares_read_only_annotations_and_an_output_schema() {
        // Every caller-visible operation retrieves local results and returns structured output.
        // Automatic derived-index maintenance is an internal lifecycle concern documented by
        // read_only_tool_annotations(), not a mutation of provider transcripts or user config.
        // Assert both protocol invariants over the whole list so a future tool cannot silently
        // fall back to destructive or opaque client assumptions.
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let v = handle_tools_list(Some(json!(1)), &config);
        let tools = v["result"]["tools"].as_array().unwrap();
        assert!(!tools.is_empty(), "server advertises at least one tool");
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            let annotations = &tool["annotations"];
            assert!(
                annotations.is_object(),
                "{name} advertises tool annotations"
            );
            assert_eq!(
                annotations["readOnlyHint"],
                json!(true),
                "{name} advertises readOnlyHint=true for its caller-visible retrieval operation"
            );
            assert_eq!(
                annotations["openWorldHint"],
                json!(false),
                "{name} advertises openWorldHint=false: its domain is the closed local index"
            );
            assert_eq!(
                tool["outputSchema"]["type"],
                json!("object"),
                "{name} advertises an object outputSchema so structuredContent is verifiable"
            );
        }
    }

    #[test]
    fn a_huge_context_window_saturates_to_the_whole_session_instead_of_overflowing() {
        // seq + after used to wrap on i64::MAX, turning "give me maximum context" into a
        // negative BETWEEN bound that silently matched nothing (release) or panicked
        // (debug). Saturating arithmetic must widen the window to the whole session.
        let (_dir, db) = fixture();
        let rows = db
            .message_context("claude:test1", 1, i64::MAX, i64::MAX)
            .expect("saturated context window reads the whole session");
        assert_eq!(
            rows.len(),
            3,
            "an oversized context request returns every message in the session"
        );
    }

    #[test]
    fn every_enum_parameter_declares_its_accepted_tokens_where_clients_keep_them() {
        // A caller binding an enum value has to be able to learn the accepted tokens. This test
        // used to require the description to restate every one of them, on the reasoning that a
        // client might strip the keyword. Reading the clients settled that: `enum` is one of the
        // fourteen keys Codex models, and VS Code's normalizer preserves it too, while the keys
        // those clients do delete are `default`, `minimum`, `maximum`, `minLength`, `minItems`,
        // `uniqueItems` and `const`. So the tokens do reach the model from `enum` alone, and a
        // prose copy is a second list free to drift from the first while costing bytes in the
        // one channel with a hard budget.
        //
        // What still has to hold is that the tokens are declared somewhere a client keeps, and
        // are not empty. A parameter with an empty or absent enum accepts anything the type
        // allows and says so nowhere.
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let v = handle_tools_list(Some(json!(1)), &config);
        let tools = v["result"]["tools"].as_array().unwrap();
        let mut enums_checked = 0;
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            let properties = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name} inputSchema has properties"));
            for (param, spec) in properties {
                for enum_values in [&spec["enum"], &spec["items"]["enum"]] {
                    let Some(tokens) = enum_values.as_array() else {
                        continue;
                    };
                    enums_checked += 1;
                    assert!(
                        !tokens.is_empty(),
                        "{name}.{param} declares an empty enum, which accepts nothing and \
                         teaches nothing"
                    );
                    for token in tokens {
                        let token = token
                            .as_str()
                            .unwrap_or_else(|| panic!("{name}.{param} enum token is not text"));
                        assert!(
                            !token.is_empty(),
                            "{name}.{param} declares an empty accepted token"
                        );
                    }
                }
                // The tokens survive Codex's normalization, which is what makes the prose copy
                // unnecessary. Measure that rather than assuming it.
                if spec.get("enum").is_some() {
                    let visible = crate::mcp_schema_budget::codex_visible_schema(spec);
                    assert_eq!(
                        visible["enum"], spec["enum"],
                        "{name}.{param}: Codex drops this parameter's accepted values, so they \
                         would have to be restated in prose"
                    );
                }
            }
        }
        assert!(
            enums_checked >= 11,
            "expected the advertised catalog to keep its enum parameters; found {enums_checked}"
        );
    }

    /// Collect the top-level property names an outputSchema object declares.
    fn output_schema_property_names(tool: &Value) -> std::collections::BTreeSet<String> {
        tool["outputSchema"]["properties"]
            .as_object()
            .expect("outputSchema has properties")
            .keys()
            .cloned()
            .collect()
    }

    #[test]
    fn search_sessions_returns_structured_hits_mirroring_cli_json() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool("search_sessions", json!({ "query": "Proj" }), &config, &db);
        let result = &response["result"];
        assert!(result["isError"].as_bool() != Some(true), "{response}");

        // Human-readable text is preserved (markdown digest, not the JSON blob).
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert!(
            text.contains("claude:test1"),
            "text digest names the hit: {text}"
        );
        assert!(
            serde_json::from_str::<Value>(text).is_err(),
            "search_sessions text stays a human digest, not JSON"
        );

        let structured = &result["structuredContent"];
        assert_eq!(structured["returned"], 1);
        assert_eq!(structured["has_more"], false);
        let hit = &structured["sessions"][0];
        // Element shape mirrors `aise search --format json`: flattened record + search fields.
        assert_eq!(hit["id"], "claude:test1");
        assert_eq!(hit["provider"], "claude");
        assert!(hit["score"].is_number(), "hit carries a numeric score");
        assert!(
            hit["match_source"].is_string(),
            "hit names its match_source"
        );
        assert!(
            hit.get("match_snippet").is_some(),
            "hit carries a match_snippet"
        );

        // Every runtime field is declared by the advertised outputSchema (no undocumented keys).
        let tools = handle_tools_list(None, &config)["result"]["tools"].clone();
        let search_sessions = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "search_sessions")
            .expect("search_sessions advertised")
            .clone();
        let declared = output_schema_property_names(&search_sessions);
        assert!(
            declared.contains("sessions") && declared.contains("returned"),
            "{declared:?}"
        );
        let hit_props: std::collections::BTreeSet<String> = search_sessions["outputSchema"]
            ["properties"]["sessions"]["items"]["properties"]
            .as_object()
            .expect("hit item schema properties")
            .keys()
            .cloned()
            .collect();
        for field in hit.as_object().expect("hit object").keys() {
            assert!(
                hit_props.contains(field),
                "runtime search_sessions hit field {field} is absent from outputSchema"
            );
        }
    }

    #[test]
    fn list_sessions_returns_structured_records() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool("list_sessions", json!({}), &config, &db);
        let result = &response["result"];
        assert!(result["isError"].as_bool() != Some(true), "{response}");
        let structured = &result["structuredContent"];
        assert_eq!(structured["returned"], 1);
        assert_eq!(structured["has_more"], false);
        assert!(structured["next_offset"].is_null());
        assert_eq!(structured["sessions"][0]["id"], "claude:test1");
        // Text digest is preserved and is not the JSON blob.
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert!(text.contains("claude:test1"));
        assert!(serde_json::from_str::<Value>(text).is_err());
    }

    #[test]
    fn list_sessions_numeric_pages_concatenate_in_database_order() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        for (id, updated_at) in [
            ("claude:newest", "2099-01-01T00:00:00Z"),
            ("claude:oldest", "2000-01-01T00:00:00Z"),
        ] {
            insert_list_session(&db, id, updated_at);
        }
        let all = call_tool("list_sessions", json!({ "limit": 0 }), &config, &db)["result"]
            ["structuredContent"]["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|session| session["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        let mut paged = Vec::new();
        for offset in 0..all.len() {
            let page = call_tool(
                "list_sessions",
                json!({ "limit": 1, "offset": offset }),
                &config,
                &db,
            );
            let structured = &page["result"]["structuredContent"];
            paged.push(
                structured["sessions"][0]["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            );
            assert_eq!(structured["has_more"], offset + 1 < all.len());
            if offset + 1 < all.len() {
                assert_eq!(structured["next_offset"], offset + 1);
            } else {
                assert!(structured["next_offset"].is_null());
            }
        }
        assert_eq!(paged, all);
    }

    #[test]
    fn search_sessions_reports_when_a_ranked_bound_omits_matches() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        insert_list_session(&db, "claude:second", "2026-01-01T00:00:00Z");

        let response = call_tool(
            "search_sessions",
            json!({ "query": "Proj", "limit": 1 }),
            &config,
            &db,
        );
        let structured = &response["result"]["structuredContent"];
        assert_eq!(structured["returned"], 1);
        assert_eq!(structured["has_more"], true);
        assert_eq!(structured["next_offset"], 1);
        assert_eq!(structured["pagination"]["consistency"], "per-call");
    }

    /// Serializes tests that mutate the process `PATH` so they never race the same env var
    /// across `cargo test`'s parallel test threads. This crate's test suite has no other test
    /// that reads or writes the real `PATH` (only this one exercises `which`-backed resolution
    /// against the live environment), so this mutex is the only coordination required.
    static PATH_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Prepends a directory containing one fake, always-findable `name` executable to `PATH`,
    /// runs `f`, then restores the original `PATH` even if `f` panics. Lets a test exercise the
    /// real [`crate::util::resume_plan`]/`which` resolution without depending on `claude`,
    /// `codex`, or `pi` actually being installed on the host or CI runner.
    fn with_stub_binary_on_path<T>(name: &str, f: impl FnOnce() -> T) -> T {
        let _guard = PATH_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let stub_dir = tempfile::tempdir().unwrap();
        let stub = stub_dir.path().join(name);
        std::fs::write(&stub, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let original_path = std::env::var_os("PATH");
        let mut search_dirs = vec![stub_dir.path().to_path_buf()];
        if let Some(existing) = &original_path {
            search_dirs.extend(std::env::split_paths(existing));
        }
        let new_path = std::env::join_paths(search_dirs).unwrap();
        // SAFETY: serialized by PATH_MUTEX above, and no other test in this crate reads or
        // writes the real PATH env var, so no concurrent access races this mutation.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        struct RestorePath(Option<std::ffi::OsString>);
        impl Drop for RestorePath {
            fn drop(&mut self) {
                // SAFETY: see above; runs on unwind too, so a panic in `f` never leaves PATH
                // mutated for later tests.
                unsafe {
                    match self.0.take() {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                }
            }
        }
        let _restore = RestorePath(original_path);

        f()
    }

    #[test]
    fn get_resume_command_structured_command_matches_text() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = with_stub_binary_on_path("claude", || {
            call_tool(
                "get_resume_command",
                json!({ "session_id": "claude:test1" }),
                &config,
                &db,
            )
        });
        let result = &response["result"];
        assert!(result["isError"].as_bool() != Some(true), "{response}");
        let text = result["content"][0]["text"].as_str().expect("text content");
        let structured = &result["structuredContent"];
        assert_eq!(structured["session_id"], "claude:test1");
        assert_eq!(
            structured["resume_command"], text,
            "structured resume_command is byte-for-byte the text content"
        );
        assert!(structured.get("cwd").is_some(), "cwd key is always present");
    }

    /// Index one user-started and one subagent session, each with a correction-shaped message.
    ///
    /// The subagent row uses text that a built-in category matches, so a test that fails to
    /// exclude it fails visibly rather than by coincidence.
    fn corrections_fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let seed = |id: &str, parent: Option<&str>, messages: &[(i64, &str, &str)]| {
            let path = format!("/x/{id}.jsonl");
            let mut parsed = minimal_record(Provider::Claude, Path::new(&path), String::new());
            parsed.session.id = format!("claude:{id}");
            parsed.session.provider_session_id = id.to_string();
            parsed.session.cwd = Some(FIXTURE_PROJECT.to_string());
            parsed.session.parent_session_id = parent.map(str::to_string);
            parsed.messages = messages
                .iter()
                .map(|(seq, ts, content)| Message {
                    seq: *seq,
                    role: Role::User,
                    ts: crate::util::parse_datetime(ts),
                    tool_name: None,
                    kind: crate::models::MessageKind::Conversation,
                    tool_call_id: None,
                    is_compaction: false,
                    content: (*content).to_string(),
                    provenance: crate::models::MessageProvenance {
                        authorship: if parent.is_some() {
                            crate::models::MessageAuthorship::Agent
                        } else {
                            crate::models::MessageAuthorship::Human
                        },
                        record_relation: crate::models::MessageRecordRelation::Original,
                        ..Default::default()
                    },
                })
                .collect();
            db.upsert_session(&parsed, 0, 0).unwrap();
        };
        seed(
            "human",
            None,
            &[
                (0, "2026-06-01T00:00:00Z", "you forgot the migration"),
                (1, "2026-06-02T00:00:00Z", "no, that's wrong"),
                (2, "2026-06-03T00:00:00Z", "also need the changelog"),
            ],
        );
        // A spawned run: these `user` rows are the CALLING AGENT's delegation prompt.
        seed(
            "spawned",
            Some("claude:human"),
            &[(0, "2026-06-04T00:00:00Z", "don't forget to run the tests")],
        );
        (dir, db)
    }

    fn run_corrections_skill(mut arguments: Value, config: &Config, db: &Db) -> Value {
        arguments
            .as_object_mut()
            .expect("skill arguments are an object")
            .entry("skill")
            .or_insert_with(|| json!({ "name": "corrections" }));
        let response = call_tool("run_skill_capability", arguments, config, db);
        assert!(
            response["result"]["isError"].as_bool() != Some(true),
            "{response}"
        );
        response["result"]["structuredContent"].clone()
    }

    /// Every response names the rules that produced it, so a result stays attributable after a
    /// policy is edited -- and an empty `matches` list can be told apart from "no rules ran".
    #[test]
    fn run_skill_reports_the_policy_and_package_that_ran_beside_its_matches() {
        let (dir, db) = corrections_fixture();
        let config = config_for_fixture(&dir);
        let result = run_corrections_skill(json!({}), &config, &db);
        let output_schema = run_skill_capability_output_schema();
        validate_schema_value(
            &result,
            &output_schema,
            &output_schema,
            "run_skill_capability",
            "structuredContent",
        )
        .expect("the emitted result must satisfy its advertised output schema");

        for (field, invalid) in [
            ("requested_selector", json!({})),
            ("selected_location", json!({ "kind": "path" })),
            (
                "execution_source",
                json!({
                    "kind": "embedded",
                    "canonical_capability_toml": "/unexpected"
                }),
            ),
        ] {
            let mut malformed = result.clone();
            let target = if field == "requested_selector" {
                &mut malformed["run"][field]
            } else {
                &mut malformed["run"]["resolved_skill"][field]
            };
            *target = invalid;
            assert!(
                validate_schema_value(
                    &malformed,
                    &output_schema,
                    &output_schema,
                    "run_skill_capability",
                    "structuredContent",
                )
                .is_err(),
                "the output schema must reject malformed {field}: {malformed:#}"
            );
        }

        let classification = &result["run"]["output"]["result"];
        let policies = classification["report"]["policies"]
            .as_array()
            .expect("policies array");
        assert_eq!(policies.len(), 1, "{result:#}");
        assert_eq!(policies[0]["name"], "corrections");
        assert_eq!(
            policies[0]["sha256"].as_str().map(str::len),
            Some(64),
            "a receipt without a digest cannot reproduce a run"
        );

        assert_eq!(result["run"]["resolved_skill"]["name"], "corrections");
        assert_eq!(classification["receipt"]["name"], "corrections");
        let matches = classification["report"]["matches"]
            .as_array()
            .expect("matches array");
        assert_eq!(
            matches
                .iter()
                .map(|hit| (
                    hit["classification"]["policy_name"].as_str().unwrap(),
                    hit["classification"]["category"].as_str().unwrap(),
                    hit["classification"]["matched_text"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("corrections", "incomplete", "also need"),
                ("corrections", "misunderstanding", "no, that's"),
                ("corrections", "skip_step", "you forgot"),
            ],
            "newest first, each naming the policy that classified it: {result:#}"
        );
        assert_eq!(result["returned"], 3);
        assert_eq!(
            result["pagination"]["ordering"],
            "timestamp desc, session id asc, sequence asc"
        );
    }

    #[test]
    fn run_skill_accepts_a_typed_direct_definition_without_changing_skill_identity() {
        let (dir, db) = corrections_fixture();
        let config = config_for_fixture(&dir);
        let result = run_corrections_skill(
            json!({
                "definition": {
                    "categories": [{
                        "name": "direct-rule",
                        "patterns": [r"\bwrong\b"]
                    }]
                }
            }),
            &config,
            &db,
        );

        assert_eq!(result["run"]["resolved_skill"]["name"], "corrections");
        assert_eq!(
            result["run"]["resolved_skill"]["execution_source"]["kind"],
            "inline"
        );
        let classification = &result["run"]["output"]["result"];
        assert_eq!(classification["receipt"]["name"], "corrections");
        assert_eq!(
            classification["report"]["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|matched| (
                    matched["classification"]["category"].as_str().unwrap(),
                    matched["classification"]["matched_text"].as_str().unwrap()
                ))
                .collect::<Vec<_>>(),
            vec![("direct-rule", "wrong")],
            "the direct categories replace the embedded rules rather than merging with them"
        );
        validate_schema_value(
            &result,
            &run_skill_capability_output_schema(),
            &run_skill_capability_output_schema(),
            "run_skill_capability",
            "structuredContent",
        )
        .expect("the inline provenance variant must satisfy the advertised output schema");
    }

    #[test]
    fn run_skill_rejects_an_oversized_typed_direct_definition() {
        let (dir, db) = corrections_fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool(
            "run_skill_capability",
            json!({
                "skill": { "name": "corrections" },
                "definition": {
                    "categories": [{
                        "name": "oversized",
                        "patterns": [format!("(?x){}needle", " ".repeat(1024 * 1024))]
                    }]
                }
            }),
            &config,
            &db,
        );

        assert_eq!(response["result"]["isError"], true, "{response:#}");
        let message = response["result"]["content"][0]["text"]
            .as_str()
            .expect("MCP tool error text");
        assert!(
            message.contains("1 MiB") && message.contains("typed inline rules"),
            "{message}"
        );
    }

    #[test]
    fn run_skill_bounds_delivery_after_full_text_classification_and_keeps_exact_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let prefix = "prefix ".repeat(3_000);
        let suffix = " suffix".repeat(3_000);
        let content = format!("{prefix}you forgot{suffix}");
        let mut parsed = minimal_record(
            Provider::Claude,
            Path::new("/x/large-classification.jsonl"),
            String::new(),
        );
        parsed.session.id = "claude:large-classification".to_string();
        parsed.session.provider_session_id = "large-classification".to_string();
        parsed.messages = vec![Message {
            seq: 17,
            role: Role::User,
            ts: crate::util::parse_datetime("2026-06-05T00:00:00Z"),
            tool_name: None,
            kind: crate::models::MessageKind::Conversation,
            tool_call_id: None,
            is_compaction: false,
            content: content.clone(),
            provenance: crate::models::MessageProvenance {
                authorship: crate::models::MessageAuthorship::Human,
                record_relation: crate::models::MessageRecordRelation::Original,
                ..Default::default()
            },
        }];
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = config_for_fixture(&dir);

        let compact = run_corrections_skill(json!({}), &config, &db);
        let compact_match = &compact["run"]["output"]["result"]["report"]["matches"][0];
        assert_eq!(
            compact_match["message_ref"],
            json!({
                "session_id": "claude:large-classification",
                "message_seq": 17
            })
        );
        assert!(
            compact_match.get("content").is_none(),
            "MCP must not retain a second unbounded whole-message field: {compact_match:#}"
        );
        let field_text = compact_match["presentation"]["field_view"]["text"]
            .as_str()
            .unwrap();
        assert!(
            field_text.chars().count() <= config.mcp.preview_chars,
            "the default field view must obey the configured MCP budget"
        );
        assert_eq!(
            compact_match["presentation"]["field_view"]["additional_field_text"],
            "after"
        );
        let match_text = compact_match["presentation"]["match_view"]["text"]
            .as_str()
            .unwrap();
        assert!(
            match_text.contains("you forgot")
                && match_text.chars().count() <= DEFAULT_MATCH_EVIDENCE_MAX_CHARS,
            "the bounded match-centered view must contain the actual regex occurrence even when \
             the field boundary view ends thousands of characters earlier: {match_text:?}"
        );
        assert_eq!(
            compact_match["classification"]["matched_text"],
            "you forgot"
        );
        validate_schema_value(
            &compact,
            &run_skill_capability_output_schema(),
            &run_skill_capability_output_schema(),
            "run_skill_capability",
            "compact structuredContent",
        )
        .unwrap();

        let minimal = run_corrections_skill(
            json!({ "match_view": { "kind": "minimal_span" } }),
            &config,
            &db,
        );
        assert_eq!(
            minimal["run"]["output"]["result"]["report"]["matches"][0]["presentation"]
                ["match_view"]["text"],
            "you forgot",
            "minimal_span must return the complete match with no unrelated surrounding text"
        );

        let full = run_corrections_skill(json!({ "detail": "full" }), &config, &db);
        let full_match = &full["run"]["output"]["result"]["report"]["matches"][0];
        assert_eq!(
            full_match["presentation"]["field_view"]["text"], content,
            "an explicit full preset retains focused whole-message recovery on MCP"
        );
        assert_eq!(
            compact_match["classification"], full_match["classification"],
            "presentation budgets cannot change classification identity or exact coordinates"
        );
        assert_eq!(
            compact["run"]["output"]["result"]["receipt"],
            full["run"]["output"]["result"]["receipt"],
            "presentation budgets cannot change the compiled-policy digest"
        );
        validate_schema_value(
            &full,
            &run_skill_capability_output_schema(),
            &run_skill_capability_output_schema(),
            "run_skill_capability",
            "full structuredContent",
        )
        .unwrap();

        let conflict = call_tool(
            "run_skill_capability",
            json!({
                "skill": { "name": "corrections" },
                "detail": "full",
                "field_view": { "kind": "max_chars", "max_chars": 10 }
            }),
            &config,
            &db,
        );
        assert_eq!(conflict["result"]["isError"], true);
        assert!(
            conflict["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("detail conflicts with field_view"),
            "{conflict:#}"
        );
    }

    /// A correction is what a PERSON told the agent. In a spawned run the `user` rows are the
    /// calling agent's delegation prompt, and those outnumber user-started sessions ~5:1.
    #[test]
    fn run_skill_scans_user_started_sessions_unless_asked_for_more() {
        let (dir, db) = corrections_fixture();
        let config = config_for_fixture(&dir);

        let contents = |arguments: Value| -> Vec<String> {
            run_corrections_skill(arguments, &config, &db)["run"]["output"]["result"]["report"]
                ["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|hit| {
                    hit["presentation"]["field_view"]["text"]
                        .as_str()
                        .unwrap()
                        .to_string()
                })
                .collect()
        };

        assert!(
            !contents(json!({}))
                .iter()
                .any(|text| text.contains("don't forget")),
            "an orchestrator prompt is not a human correction"
        );
        assert!(
            !contents(json!({ "session_kinds": ["user", "subagent"] }))
                .iter()
                .any(|text| text.contains("don't forget")),
            "session scope cannot override source-authorship evidence"
        );
        assert!(contents(json!({ "session_kinds": ["subagent"] })).is_empty());
    }

    /// `limit` asks for one page and `all_results` asks for every match; accepting both would
    /// mean silently honoring one and dropping the other.
    #[test]
    fn run_skill_pages_exactly_and_refuses_contradictory_paging() {
        let (dir, db) = corrections_fixture();
        let config = config_for_fixture(&dir);

        let first = run_corrections_skill(json!({ "limit": 2 }), &config, &db);
        assert_eq!(first["returned"], 2);
        assert_eq!(
            first["next_offset"], 2,
            "a full page reports where to continue: {first:#}"
        );

        let second = run_corrections_skill(json!({ "limit": 2, "offset": 2 }), &config, &db);
        assert_eq!(second["returned"], 1);
        assert_eq!(
            second["next_offset"],
            Value::Null,
            "a short page is the only proof of the end: {second:#}"
        );
        assert_eq!(
            second["run"]["output"]["result"]["report"]["matches"][0]["classification"]
                ["matched_text"],
            "you forgot"
        );

        let exact_final = run_corrections_skill(json!({ "limit": 3 }), &config, &db);
        assert_eq!(exact_final["returned"], 3);
        assert_eq!(
            exact_final["next_offset"],
            Value::Null,
            "an exact-size final page must not advertise a nonexistent continuation"
        );

        let every = run_corrections_skill(json!({ "all_results": true }), &config, &db);
        assert_eq!(every["returned"], 3);
        assert_eq!(
            every["pagination"]["limit"],
            Value::Null,
            "all_results means there was no page size, not a page size of zero"
        );

        let conflict = call_tool(
            "run_skill_capability",
            json!({
                "skill": { "name": "corrections" },
                "limit": 2,
                "all_results": true
            }),
            &config,
            &db,
        );
        assert_eq!(conflict["result"]["isError"], true);
        let text = conflict["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("limit or all_results, not both"),
            "the error must say which two arguments disagree: {text}"
        );
    }

    /// An unknown skill must fail rather than quietly answering with the default rules, which
    /// would look like a successful run against rules the caller never selected.
    #[test]
    fn run_skill_rejects_an_unknown_skill_instead_of_using_the_defaults() {
        let (dir, db) = corrections_fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool(
            "run_skill_capability",
            json!({ "skill": { "name": "not-installed" } }),
            &config,
            &db,
        );
        assert_eq!(response["result"]["isError"], true);
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("not-installed") && text.contains("catalog"),
            "name the value and where to find valid ones: {text}"
        );
    }

    #[test]
    fn run_skill_path_requires_an_explicit_configured_discovery_root() {
        let (dir, db) = corrections_fixture();
        let packages = dir.path().join("packages");
        let skill = packages.join("team-rules");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: team-rules\ndescription: fixture\nmetadata:\n  version: 1.0.0\n---\n",
        )
        .unwrap();
        std::fs::write(
            skill.join("capability.toml"),
            crate::corrections::EMBEDDED_POLICY_TOML,
        )
        .unwrap();

        let mut config = config_for_fixture(&dir);
        let denied = call_tool(
            "run_skill_capability",
            json!({ "skill": { "path": skill.clone() } }),
            &config,
            &db,
        );
        assert_eq!(denied["result"]["isError"], true);
        assert!(
            denied["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("outside configured [skills].search_paths"),
            "{denied:#}"
        );

        config.skills.search_paths = vec![packages.to_string_lossy().into_owned()];
        let allowed = run_corrections_skill(json!({ "skill": { "path": skill } }), &config, &db);
        assert_eq!(allowed["run"]["resolved_skill"]["name"], "team-rules");
        assert_eq!(
            allowed["run"]["resolved_skill"]["selected_location"]["kind"],
            "path"
        );
    }

    /// The advertised schema must be usable without a trial call: an agent reading it needs to
    /// know the default page size, that skill names are runtime-dependent, and that the
    /// session-class default differs from the other tools'.
    #[test]
    fn run_skill_capability_schema_documents_boundary_defaults_and_divergence() {
        let (dir, db) = fixture();
        let _ = db;
        let config = config_for_fixture(&dir);
        let tools = handle_tools_list(Some(json!(1)), &config)["result"]["tools"].clone();
        let tool = tools
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "run_skill_capability")
            .expect("run_skill_capability is advertised")
            .clone();

        assert_eq!(tool["annotations"]["readOnlyHint"], true);
        assert!(
            tool["outputSchema"]["oneOf"][0]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("run")),
            "provenance is required, not optional: {tool:#}"
        );
        let description = tool["description"].as_str().unwrap();
        assert!(
            description.contains("aise skills corrections"),
            "the CLI verb stays findable from the tool description: {description}"
        );
        assert!(
            description.contains("deterministic")
                && description.contains("capability")
                && description.contains("By default Aise reads")
                && description.contains("definition can supply typed categories")
                && description.contains("SKILL.md")
                && description.contains("MCP client or AI harness"),
            "the description must distinguish deterministic capability execution from the \
             harness-owned AI instructions: {description}"
        );
        assert!(
            !description.contains("run a skill") && !description.contains("Run one"),
            "the description must not claim that Aise executes AI skill instructions: \
             {description}"
        );

        let properties = &tool["inputSchema"]["properties"];
        assert_eq!(
            properties["definition"]["properties"]["categories"]["minItems"], 1,
            "a direct definition must advertise its nonempty category invariant"
        );
        assert_eq!(
            properties["limit"]["default"],
            json!(config.mcp.run_message_classification_limit),
            "the advertised default must be the configured one"
        );
        let skill = properties["skill"]["description"].as_str().unwrap();
        assert!(
            skill.contains("name") && skill.contains("path"),
            "skill names are runtime-dependent, so the schema points at the command that lists \
             them rather than freezing a stale list: {skill}"
        );
        assert!(
            properties["skill"].get("enum").is_none(),
            "a JSON-Schema enum of skill names would go stale the moment one is installed"
        );
        let kinds = properties["session_kinds"]["description"].as_str().unwrap();
        assert!(
            kinds.contains("differs from search_messages"),
            "a default that differs from the sibling tools must say so, or it is a trap: {kinds}"
        );

        for invalid in [
            json!({}),
            json!({ "name": "", }),
            json!({ "name": "corrections", "path": "./corrections" }),
        ] {
            let error = validate_tool_call(
                &json!({ "name": "run_skill_capability", "arguments": { "skill": invalid } }),
                &tools,
            )
            .expect_err("malformed selectors must fail before the index opens");
            assert!(
                error.contains("exactly one") || error.contains("at least 1"),
                "{error}"
            );
        }
        let duplicate = validate_tool_call(
            &json!({
                "name": "run_skill_capability",
                "arguments": {
                    "skill": { "name": "corrections" },
                    "additional_skills": [
                        { "name": "other" },
                        { "name": "other" }
                    ]
                }
            }),
            &tools,
        )
        .expect_err("duplicate additional selectors must fail before the index opens");
        assert!(duplicate.contains("duplicates an earlier"), "{duplicate}");
    }

    #[test]
    fn tools_list_exposes_expected_tools_each_with_a_schema() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let v = handle_tools_list(Some(json!(1)), &config);
        let tools = v["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            vec![
                "search_sessions",
                "get_session",
                "list_sessions",
                "get_resume_command",
                "search_messages",
                "run_skill_capability",
                "get_index_status",
                "query_session_index",
            ]
        );
        let removed_analysis = call_tool("analyze_sessions", json!({}), &config, &db);
        assert_eq!(removed_analysis["result"]["isError"], true);
        // A caller that names a removed or mistyped tool must be able to recover from the error
        // text alone, so it names the unknown tool and then every tool this server does serve.
        let removed_text = removed_analysis["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        assert!(
            removed_text.starts_with("unknown tool: analyze_sessions — this server provides "),
            "{removed_text}"
        );
        for served in [
            "search_sessions",
            "get_session",
            "list_sessions",
            "get_resume_command",
            "search_messages",
            "run_skill_capability",
            "get_index_status",
            "query_session_index",
        ] {
            assert!(
                removed_text.contains(&format!("{served:?}")),
                "{served} missing from {removed_text}"
            );
        }
        // An unknown tool far from every served name still lists the catalogue and must NOT
        // invent a suggestion — a confidently wrong pointer is worse than none.
        let far_miss = call_tool("frobnicate_widgets", json!({}), &config, &db);
        let far_miss_text = far_miss["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        assert!(
            !far_miss_text.contains("did you mean"),
            "no suggestion for a distant name: {far_miss_text}"
        );
        assert!(
            far_miss_text.contains(r#""search_messages""#),
            "catalogue still listed: {far_miss_text}"
        );

        // A near-miss tool name gets the same treatment a near-miss parameter name already gets:
        // lead with the likeliest intended tool, then still list the catalogue so a caller whose
        // guess was wrong can recover from the one message.
        let near_miss = call_tool("search_message", json!({}), &config, &db);
        let near_miss_text = near_miss["result"]["content"][0]["text"]
            .as_str()
            .expect("error text");
        assert!(
            near_miss_text.contains(r#"did you mean "search_messages"?"#),
            "{near_miss_text}"
        );
        assert!(
            near_miss_text.contains(r#""get_session""#),
            "catalogue still listed: {near_miss_text}"
        );

        // Every advertised tool must carry an object inputSchema and a non-empty description
        // (clients rely on both to choose and call the tool).
        for t in tools {
            assert_eq!(
                t["inputSchema"]["type"], "object",
                "tool {} schema",
                t["name"]
            );
            assert_eq!(
                t["inputSchema"]["additionalProperties"], false,
                "tool {} must reject misspelled arguments",
                t["name"]
            );
            assert!(t["description"].as_str().is_some_and(|d| !d.is_empty()));
        }
        let get_session = tools
            .iter()
            .find(|t| t["name"] == "get_session")
            .expect("get_session advertised");
        let search_messages = tools
            .iter()
            .find(|t| t["name"] == "search_messages")
            .expect("search_messages advertised");
        let expected_providers: Vec<_> = crate::source::PROVIDERS
            .into_iter()
            .map(|provider| provider.as_str())
            .collect();
        for tool_name in ["search_sessions", "list_sessions"] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} advertised"));
            assert_eq!(
                tool["inputSchema"]["properties"]["provider"]["enum"],
                json!(expected_providers),
                "{tool_name} provider enum must match the canonical registry"
            );
        }
        let message_providers = &search_messages["inputSchema"]["properties"]["providers"];
        assert_eq!(
            message_providers["items"]["enum"],
            json!(expected_providers),
            "search_messages provider set must match the canonical registry"
        );
        assert_eq!(message_providers["minItems"], 1);
        assert_eq!(message_providers["uniqueItems"], true);
        for tool_name in ["search_sessions", "list_sessions"] {
            let tool = tools
                .iter()
                .find(|tool| tool["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} advertised"));
            let limit_description = tool["inputSchema"]["properties"]["limit"]["description"]
                .as_str()
                .expect("limit description");
            assert!(
                limit_description.contains("Set 0 only to explicitly request all"),
                "{tool_name} must make its unbounded response request explicit"
            );
        }
        let search_description = tools
            .iter()
            .find(|tool| tool["name"] == "search_sessions")
            .unwrap()["description"]
            .as_str()
            .expect("search_sessions description");
        for provider in crate::source::PROVIDERS {
            let concrete_label = format!(
                "{} (provider={})",
                provider.display_name(),
                provider.as_str()
            );
            assert!(
                search_description.contains(&concrete_label),
                "search_sessions description must contain {concrete_label}: {search_description}"
            );
            for tool_name in ["search_sessions", "list_sessions"] {
                let tool = tools
                    .iter()
                    .find(|tool| tool["name"] == tool_name)
                    .unwrap_or_else(|| panic!("{tool_name} advertised"));
                assert!(
                    tool["inputSchema"]["properties"]["provider"]["description"]
                        .as_str()
                        .is_some_and(|description| description.contains(&concrete_label)),
                    "{tool_name} provider help must contain {concrete_label}"
                );
            }
            assert!(
                message_providers["description"]
                    .as_str()
                    .is_some_and(|description| description
                        .to_lowercase()
                        .contains("session sources")),
                "search_messages provider-set help must explain the selected source set"
            );
        }
        for tool in tools {
            let description = tool["description"]
                .as_str()
                .unwrap_or_else(|| panic!("{} description is a string", tool["name"]));
            assert!(
                !description.trim().is_empty(),
                "{} description is nonempty",
                tool["name"]
            );
        }
        let query_session_index = tools
            .iter()
            .find(|t| t["name"] == "query_session_index")
            .expect("query_session_index advertised");
        assert!(
            !query_session_index["description"]
                .as_str()
                .expect("query_session_index description")
                .contains("objects.."),
            "schema fallback punctuation must be normalized"
        );
        for tool in [get_session, search_messages, query_session_index] {
            assert_eq!(
                tool["outputSchema"]["type"], "object",
                "machine-readable MCP tool {} advertises object output",
                tool["name"]
            );
        }
        for tool in [search_messages, query_session_index] {
            assert_eq!(
                tool["outputSchema"]["additionalProperties"], false,
                "{} must advertise a closed top-level output envelope",
                tool["name"]
            );
        }
        let output_properties = &search_messages["outputSchema"]["properties"];
        for field in [
            "response_schema_version",
            "effective_request",
            "results",
            "page",
            "included",
            "receipt",
        ] {
            assert!(
                output_properties.get(field).is_some(),
                "search_messages output schema must document canonical field {field}"
            );
        }
        for removed in [
            "query",
            "hits",
            "pagination",
            "presentation",
            "search_explanation",
            "origins",
            "sessions",
        ] {
            assert!(
                output_properties.get(removed).is_none(),
                "search_messages output schema must not retain provisional field {removed}"
            );
        }
        let result_schema = &output_properties["results"]["items"];
        assert_eq!(
            result_schema["additionalProperties"], false,
            "search_messages results must advertise every runtime field"
        );
        for field in [
            "message_ref",
            "message_metadata",
            "match",
            "presentation",
            "included",
            "context",
        ] {
            assert!(
                result_schema["properties"].get(field).is_some(),
                "search_messages result schema must document {field}"
            );
        }
        // Named shapes are followed rather than read in place. Closedness is the property under
        // test, and it survives extraction; reading `additionalProperties` off a `$ref` node
        // would report Null and turn a naming change into a false contract failure.
        let resolved = |schema: &Value| -> Value {
            match schema.get("$ref").and_then(Value::as_str) {
                Some(pointer) => {
                    let name = pointer
                        .strip_prefix("#/$defs/")
                        .unwrap_or_else(|| panic!("{pointer} is not a local $defs pointer"));
                    search_messages["outputSchema"]["$defs"][name].clone()
                }
                None => schema.clone(),
            }
        };
        assert_eq!(
            resolved(&result_schema["properties"]["message_ref"])["additionalProperties"],
            false
        );
        assert_eq!(
            result_schema["properties"]["presentation"]["additionalProperties"],
            false
        );
        assert_eq!(
            result_schema["properties"]["context"]["additionalProperties"],
            false
        );
        assert_eq!(output_properties["page"]["additionalProperties"], false);
        // The three searchable fields are `field`'s accepted values, and they travel as `enum`,
        // which every measured client preserves. This used to require the tool description to
        // restate them; what the tool description owes instead is what a token cannot carry --
        // what the tool returns and how to expand one hit.
        assert_eq!(
            search_messages["inputSchema"]["properties"]["field"]["enum"],
            json!(["content", "tool_name", "tool_argument"])
        );
        assert!(search_messages["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("structuredContent is authoritative")
                    && description.contains("get_session(session_id, message_seq)")
            }));
        // This filter applies to the canonical tool name whatever `field` is searching, so its
        // description has to name that field. The registry writes it surface-neutrally as "tool
        // name" because the CLI spells the flag `--tool-name-contains`; accept either spelling
        // rather than requiring the MCP token in prose shared by four surfaces.
        assert!(
            search_messages["inputSchema"]["properties"]["tool_name_contains"]["description"]
                .as_str()
                .is_some_and(|description| description
                    .to_lowercase()
                    .replace('_', " ")
                    .contains("tool name")),
            "{}",
            search_messages["inputSchema"]["properties"]["tool_name_contains"]["description"]
        );
        assert_eq!(output_properties["receipt"]["additionalProperties"], false);
        let origins_schema = &output_properties["receipt"]["properties"]["parameter_origins"];
        assert_eq!(origins_schema["type"], json!(["object", "null"]));
        assert_eq!(origins_schema["additionalProperties"], false);
        for field in [
            "result_extent",
            "context_messages_before",
            "context_messages_after",
            "includes",
            "detail",
            "lines_per_message",
            "field_view",
            "match_view",
            "receipt_level",
            "result_order",
        ] {
            assert_eq!(
                origins_schema["properties"][field]["additionalProperties"], false,
                "origin schema for {field} is closed"
            );
        }
        assert_eq!(output_properties["included"]["additionalProperties"], false);
        assert!(get_session["outputSchema"]["oneOf"]
            .as_array()
            .is_some_and(|variants| variants
                .iter()
                .all(|variant| variant["additionalProperties"] == false)));
        let get_session_variants = get_session["outputSchema"]["oneOf"]
            .as_array()
            .expect("get_session output variants");
        // Same as above: follow a named shape to the definition that carries its closedness.
        let get_session_resolved = |schema: &Value| -> Value {
            match schema.get("$ref").and_then(Value::as_str) {
                Some(pointer) => {
                    let name = pointer
                        .strip_prefix("#/$defs/")
                        .unwrap_or_else(|| panic!("{pointer} is not a local $defs pointer"));
                    get_session["outputSchema"]["$defs"][name].clone()
                }
                None => schema.clone(),
            }
        };
        assert_eq!(
            get_session_resolved(&get_session_variants[0]["properties"]["session"])
                ["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_variants[0]["properties"]["transcript"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_resolved(&get_session_variants[1]["properties"]["session_metadata"])
                ["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_variants[1]["properties"]["messages"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_variants[2]["properties"]["session"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_variants[2]["properties"]["time_profile"]["additionalProperties"],
            false
        );
        for field in ["user_intent", "tool_activity", "refs", "changed_files"] {
            assert_eq!(
                get_session_resolved(&get_session_variants[2]["properties"][field]["items"])
                    ["additionalProperties"],
                false,
                "summary evidence schema must close {field} items"
            );
        }
        let get_index_status = tools
            .iter()
            .find(|tool| tool["name"] == "get_index_status")
            .expect("get_index_status advertised");
        assert_eq!(
            get_index_status["outputSchema"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_index_status["outputSchema"]["properties"]["parser_health"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_index_status["outputSchema"]["properties"]["parser_health"]["properties"]
                ["providers"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_index_status["outputSchema"]["properties"]["providers"]["items"]
                ["additionalProperties"],
            false
        );
        for required in [
            "db_path",
            "parser_health",
            "repairable_stale_sessions",
            "unavailable_stale_sessions",
            "unindexed_files",
            "repair_commands",
            "readiness",
            "providers",
        ] {
            assert!(get_index_status["outputSchema"]["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == required)));
        }
        assert!(
            get_index_status["outputSchema"].get("oneOf").is_none(),
            "get_index_status always runs and must not advertise a not-ready alternative"
        );
        for tool in tools
            .iter()
            .filter(|tool| tool["name"] != "get_index_status")
        {
            let alternatives = tool["outputSchema"]["oneOf"].as_array().unwrap_or_else(|| {
                panic!(
                    "{} must advertise both normal and unavailable-snapshot outcomes",
                    tool["name"]
                )
            });
            let unavailable = alternatives
                .iter()
                .find(|alternative| {
                    alternative["required"].as_array().is_some_and(|required| {
                        required.iter().any(|field| field == "operation_executed")
                    })
                })
                .unwrap_or_else(|| {
                    panic!(
                        "{} is missing its unavailable-snapshot outcome",
                        tool["name"]
                    )
                });
            let properties = unavailable
                .get("properties")
                .filter(|properties| properties.get("index_status").is_some())
                .or_else(|| tool["outputSchema"].get("properties"))
                .expect("unavailable-snapshot properties");
            assert_eq!(properties["operation_executed"]["const"], false);
            assert_eq!(
                properties["index_status"]["properties"]["snapshot"]["properties"]["availability"]
                    ["enum"],
                json!(["unavailable", "usable"])
            );
            assert_eq!(
                properties["next_call"]["properties"]["tool"]["const"],
                "get_index_status"
            );
            assert_eq!(
                properties["next_call"]["properties"]["arguments"]["maxProperties"],
                0
            );
        }
        let readiness = &get_index_status["outputSchema"]["properties"]["readiness"];
        assert_eq!(readiness["additionalProperties"], false);
        assert_eq!(
            readiness["properties"]["snapshot"]["properties"]["availability"]["enum"],
            json!(["unavailable", "usable"])
        );
        assert_eq!(
            readiness["properties"]["refresh"]["properties"]["state"]["enum"],
            json!([
                "not_started",
                "indexing",
                "fresh",
                "postponed",
                "failed_with_recovery"
            ])
        );
        let resume_description = tools
            .iter()
            .find(|tool| tool["name"] == "get_resume_command")
            .expect("get_resume_command advertised")["description"]
            .as_str()
            .expect("get_resume_command description");
        for provider in crate::source::PROVIDERS {
            assert!(resume_description.contains(provider.display_name()));
        }
        assert!(resume_description.contains("cannot be resumed"));
        assert!(get_session["description"]
            .as_str()
            .is_some_and(|d| d.contains("summary=true")
                && d.contains("transcript_lines=N")
                && d.contains("message_seq=N")
                && d.contains("last 40 transcript lines")));
        assert!(query_session_index["description"]
            .as_str()
            .is_some_and(|d| {
                d.contains("Bounded live schema summary")
                    && d.contains("sessions(")
                    && d.contains("messages(")
                    && d.contains("Prefer search_messages")
                    && d.contains("SELECT/WITH")
                    && !d.contains("messages_fts(")
            }));
        // The summary above lists tool_name among the columns, so a caller can write a predicate
        // straight from this description without ever calling schema_table. Matching one
        // provider's spelling then returns a per-provider answer and no error, so the caveat has
        // to ride along with the column list that invites the mistake.
        let query_description = query_session_index["description"].as_str().unwrap();
        assert!(
            query_description.contains("tool_name"),
            "{query_description}"
        );
        assert!(
            query_description.contains("tool_name_contains"),
            "{query_description}"
        );
        assert!(query_description.contains("note"), "{query_description}");
        let sql_description = query_session_index["inputSchema"]["properties"]["sql"]
            ["description"]
            .as_str()
            .unwrap();
        assert!(sql_description.contains("raw read-only SQL"));
        assert!(sql_description.contains("Prefer search_messages"));
        assert!(query_session_index["inputSchema"]["properties"]["schema_table"].is_object());
        assert_eq!(
            get_session["inputSchema"]["properties"]["summary"]["default"], false,
            "summary is opt-in"
        );
        assert!(get_session["inputSchema"]["properties"]["transcript_lines"].is_object());
        assert!(get_session["inputSchema"]["properties"]["message_seq"].is_object());
        assert!(get_session["inputSchema"]["properties"]["seq"].is_null());
        assert!(get_session["inputSchema"]["properties"]["max_lines"].is_null());
        assert!(get_session["inputSchema"]["properties"]["view"].is_null());
        assert_eq!(
            get_session["inputSchema"]["properties"]["context"]["default"], 0,
            "context defaults to 0 unless explicitly requested"
        );
        assert_eq!(
            get_session["inputSchema"]["properties"]["transcript_lines"]["default"], -40,
            "bare get_session is bounded by default"
        );
        assert_eq!(
            search_messages["inputSchema"]["properties"]["context"]["default"], 0,
            "search hit expansion is opt-in"
        );
        let message_window = search_messages["inputSchema"]["properties"]["lines_per_message"]
            ["description"]
            .as_str()
            .unwrap();
        assert_eq!(
            search_messages["inputSchema"]["properties"]["lines_per_message"]["default"], 0,
            "per-message presentation remains uncapped until callers opt in"
        );
        // A signed count states all four cases on the parameter itself, because the sign
        // convention is what a caller cannot guess from the type.
        for required in ["positive keeps first N", "negative last N", "0 none"] {
            assert!(
                message_window.contains(required),
                "missing {required:?}: {message_window}"
            );
        }
        // The three facts this parameter shares with field_view and match_view -- the order the
        // budgets apply in, that they are presentation only, and that detail conflicts with all
        // three -- are stated once in tool.description rather than three times here. That is the
        // channel every client forwards whole and Codex never strips, and stating them once is
        // what keeps three copies from drifting apart.
        let tool_doc = search_messages["description"].as_str().unwrap();
        for required in [
            "lines_per_message applies its line window first",
            "never change matching, ordering, result count, context membership, or includes",
            "detail conflicts with lines_per_message, field_view, and match_view",
        ] {
            assert!(
                tool_doc.contains(required),
                "missing {required:?} from tool.description: {tool_doc}"
            );
        }
        assert!(search_messages["description"]
            .as_str()
            .is_some_and(|d| d.contains("message_seq") && !d.contains("session_id, seq")));
        let match_mode = &search_messages["inputSchema"]["properties"]["query_mode"];
        assert_eq!(match_mode["enum"], json!(["literal", "regex", "fuzzy"]));
        assert_eq!(match_mode["default"], "literal");
        // The default has to be in the text as well as the keyword: Codex and VS Code both
        // delete `default` before a model sees the schema, and JSON Schema 2020-12 section 9.2
        // makes it an annotation with no validation force even where it survives.
        assert!(
            match_mode["description"]
                .as_str()
                .is_some_and(|d| d.contains("omit for literal")),
            "query_mode does not name its default in prose: {}",
            match_mode["description"]
        );
        // Which regex dialect and how bounded the fuzzy match is are facts an enum token cannot
        // carry, so they are stated once in the channel every client forwards whole.
        for required in [
            "query_mode=regex is Rust regex syntax",
            "bounded fuzzy match",
        ] {
            assert!(
                tool_doc.contains(required),
                "missing {required:?} from tool.description: {tool_doc}"
            );
        }
        // Every parameter that declares a default states it in prose too, in whatever phrasing
        // the generator uses, because the keyword itself is deleted before a model sees it.
        for (parameter, expected_default) in [
            ("all_results", "false"),
            ("include_compaction", "true"),
            ("offset", "0"),
        ] {
            let description = search_messages["inputSchema"]["properties"][parameter]
                ["description"]
                .as_str()
                .unwrap();
            assert!(
                description.to_lowercase().contains("omit for")
                    && description.contains(expected_default),
                "{parameter} description does not name its {expected_default:?} default: \
                 {description}"
            );
        }
        // Three ordered levels whose contents differ, so the description says which level buys
        // which diagnostic rather than listing all three payloads without mapping them.
        let receipt_description = search_messages["inputSchema"]["properties"]["receipt_level"]
            ["description"]
            .as_str()
            .unwrap();
        assert!(
            receipt_description.contains("summary explains the plan"),
            "{receipt_description}"
        );
        assert!(
            receipt_description.contains("full adds value origins"),
            "{receipt_description}"
        );
        assert!(
            receipt_description.contains("omit for none"),
            "{receipt_description}"
        );
    }

    #[test]
    fn out_of_range_argument_explains_what_the_bound_selects() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let tools = handle_tools_list(None, &config)["result"]["tools"].clone();

        // Exercised through validate_tool_call, the entry point the JSON-RPC server uses. The
        // call_tool test helper bypasses schema validation and reaches a separate non-negative
        // guard, so it would not cover this message.
        let text = validate_tool_call(
            &json!({ "name": "search_sessions", "arguments": { "query": "x", "limit": -3 } }),
            &tools,
        )
        .unwrap_err();

        // The bound alone is not actionable for paging: 0 is a documented selection rather than
        // merely the floor, so the parameter's own description has to reach the caller.
        assert!(text.contains("must be at least 0"), "{text}");
        assert!(text.contains("Maximum sessions to return"), "{text}");
        assert!(
            text.contains("Set 0 only to explicitly request all"),
            "{text}"
        );
    }

    /// `nearest_name` backs both the unknown-parameter and unknown-tool messages, so its
    /// boundaries are pinned once here rather than twice through the surfaces above.
    #[test]
    fn nearest_name_suggests_only_within_a_length_scaled_distance() {
        let tools = [
            "search_sessions",
            "search_messages",
            "get_session",
            "list_sessions",
        ];

        // Exact and one-character misses resolve.
        assert_eq!(nearest_name("get_session", &tools), Some("get_session"));
        assert_eq!(
            nearest_name("search_message", &tools),
            Some("search_messages")
        );
        assert_eq!(nearest_name("get_sessions", &tools), Some("get_session"));

        // Empty candidate set yields no suggestion rather than panicking on an empty min.
        assert_eq!(nearest_name("anything", &[]), None);

        // An empty name must not be dragged onto the shortest candidate: distance equals that
        // candidate's length, far outside a threshold of 1.
        assert_eq!(nearest_name("", &tools), None);

        // Threshold scales with length. "abc" (len 3) tolerates distance 1 only, so a
        // two-edit gap is refused even though the names are similar in shape.
        assert_eq!(nearest_name("abc", &["abd"]), Some("abd"));
        assert_eq!(nearest_name("abc", &["axy"]), None);

        // The clamp holds at the top: however long the name, at most 3 edits are tolerated.
        // "query_session_index" plus three trailing characters is distance 3 (accepted); plus
        // four is distance 4 (refused), even though len/3 would otherwise permit 7.
        assert_eq!(
            nearest_name("query_session_indexxxx", &["query_session_index"]),
            Some("query_session_index")
        );
        assert_eq!(
            nearest_name("query_session_indexxxxx", &["query_session_index"]),
            None
        );

        // Equidistant candidates resolve deterministically to the shorter name, so the same
        // typo never produces a different suggestion between runs.
        assert_eq!(
            nearest_name("sessions", &["session", "sessionss"]),
            Some("session")
        );

        // Distance is counted in characters, not bytes: a multibyte name must neither panic nor
        // be scored as though each character were several edits.
        assert_eq!(nearest_name("sesión", &["sesion"]), Some("sesion"));
        assert_eq!(nearest_name("日本語", &["中文"]), None);
    }

    #[test]
    fn unknown_parameter_names_the_likeliest_intended_parameter_or_lists_accepted_ones() {
        let accepted = ["limit", "query", "provider", "path_prefix", "since"];

        // A typo close to exactly one accepted name resolves to that name, so the caller can fix
        // the call without re-reading the schema.
        assert_eq!(
            unknown_key_hint("limitt", &accepted),
            " — did you mean \"limit\"?"
        );
        assert_eq!(
            unknown_key_hint("provder", &accepted),
            " — did you mean \"provider\"?"
        );

        // A key with no plausible near match falls back to the complete accepted set, sorted, so
        // the message is still actionable rather than merely a rejection.
        let unrelated = unknown_key_hint("completely_different", &accepted);
        assert!(
            unrelated.starts_with(" — accepted parameters are "),
            "{unrelated}"
        );
        for name in accepted {
            assert!(unrelated.contains(&format!("{name:?}")), "{unrelated}");
        }

        // A short key must not be dragged onto an unrelated short name by a loose threshold.
        assert_eq!(unknown_key_hint("zzz", &["role", "kind"]), {
            " — accepted parameters are \"kind\", \"role\""
        });

        // No schema properties means no hint text rather than a dangling separator.
        assert_eq!(unknown_key_hint("anything", &[]), "");
    }

    #[test]
    fn advertised_schemas_reject_unknown_wrong_type_enum_and_out_of_range_arguments() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let tools = handle_tools_list(None, &config)["result"]["tools"].clone();

        for (tool, arguments, expected) in [
            (
                "search_sessions",
                json!({ "query": "x", "provder": "codex" }),
                "unknown",
            ),
            (
                "list_sessions",
                json!({ "limit": "10" }),
                "expected integer",
            ),
            (
                "get_session",
                json!({ "session_id": "x", "summary": "yes" }),
                "expected boolean",
            ),
            (
                "get_resume_command",
                json!({ "session_id": 4 }),
                "expected string",
            ),
            (
                "search_messages",
                json!({ "role": "human" }),
                "must be one of",
            ),
            (
                "search_messages",
                json!({
                    "query": "x",
                    "field_view": {"kind": "max_chars", "max_chars": 0}
                }),
                "must match exactly one schema alternative",
            ),
            (
                "search_messages",
                json!({ "query": "x", "preview_chars": 20 }),
                "unknown",
            ),
            ("search_messages", json!({ "regex": "x" }), "unknown"),
            (
                "search_messages",
                json!({ "query": "x", "query_mode": "approximate" }),
                "must be one of",
            ),
            ("get_index_status", json!({ "unexpected": true }), "unknown"),
            (
                "query_session_index",
                json!({ "offset": -1 }),
                "must be at least 0",
            ),
        ] {
            let error =
                validate_tool_call(&json!({ "name": tool, "arguments": arguments }), &tools)
                    .unwrap_err();
            assert!(
                error.contains(expected),
                "{tool} should report {expected:?}, got {error:?}"
            );
        }

        for removed_alias in ["view", "seq", "max_lines"] {
            let mut arguments = json!({ "session_id": "x" });
            arguments[removed_alias] = json!(1);
            let error = validate_tool_call(
                &json!({
                    "name": "get_session",
                    "arguments": arguments
                }),
                &tools,
            )
            .unwrap_err();
            assert!(
                error.contains("unknown") && error.contains(removed_alias),
                "removed get_session alias {removed_alias:?} must fail before index access: {error}"
            );
        }
    }

    #[test]
    fn every_advertised_provider_is_accepted_by_provider_filtered_tools() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        for provider in crate::source::PROVIDERS {
            let provider = provider.as_str();
            for (tool, arguments) in [
                (
                    "search_sessions",
                    json!({ "query": "hello", "provider": provider }),
                ),
                ("list_sessions", json!({ "provider": provider })),
                (
                    "search_messages",
                    json!({ "query": "hello", "providers": [provider] }),
                ),
            ] {
                let response = call_tool(tool, arguments, &config, &db);
                assert!(
                    response.get("result").is_some(),
                    "{tool} must accept advertised provider {provider}: {response}"
                );
                assert!(
                    response.get("error").is_none(),
                    "{tool} rejected advertised provider {provider}: {response}"
                );
            }
        }

        let response = call_tool(
            "search_sessions",
            json!({ "query": "hello", "provider": "not-a-provider" }),
            &config,
            &db,
        );
        assert_eq!(response["result"]["isError"], true);
        assert!(response["result"]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("unsupported provider: not-a-provider")));

        for arguments in [
            json!({ "query": "hello", "providers": [] }),
            json!({ "query": "hello", "providers": ["not-a-provider"] }),
        ] {
            let response = call_tool("search_messages", arguments, &config, &db);
            assert_eq!(response["result"]["isError"], true, "{response}");
        }
        let duplicate = call_tool(
            "search_messages",
            json!({ "query": "hello", "providers": ["claude", "claude"] }),
            &config,
            &db,
        );
        assert_eq!(
            duplicate["result"]["structuredContent"]["effective_request"]["provider_scope"]
                ["providers"],
            json!(["claude"]),
            "the service defensively normalizes duplicate providers"
        );
        let tools = handle_tools_list(Some(json!(1)), &config)["result"]["tools"].clone();
        let duplicate_error = validate_tool_call(
            &json!({
                "name": "search_messages",
                "arguments": { "query": "hello", "providers": ["claude", "claude"] }
            }),
            &tools,
        )
        .unwrap_err();
        assert!(duplicate_error.contains("duplicates"), "{duplicate_error}");
    }

    #[test]
    fn mcp_json_tools_match_text_while_search_messages_uses_a_bounded_summary() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        for (tool, arguments) in [
            ("query_session_index", json!({ "schema_table": "messages" })),
            (
                "get_session",
                json!({ "session_id": "claude:test1", "summary": true }),
            ),
        ] {
            let response = call_tool(tool, arguments, &config, &db);
            let result = &response["result"];
            let text = result["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{tool} text result"));
            let text_json: Value = serde_json::from_str(text)
                .unwrap_or_else(|err| panic!("{tool} text is not JSON: {err}\n{text}"));
            assert_eq!(
                result["structuredContent"], text_json,
                "{tool} structuredContent should match its JSON text content"
            );
            assert!(result["isError"].as_bool() != Some(true), "{response}");
        }

        let response = call_tool(
            "search_messages",
            json!({ "query": "hello", "limit": 1 }),
            &config,
            &db,
        );
        let result = &response["result"];
        let text = result["content"][0]["text"]
            .as_str()
            .expect("search_messages bounded text summary");
        assert!(serde_json::from_str::<Value>(text).is_err());
        assert!(text.contains("structuredContent is the authoritative response"));
        assert_eq!(result["structuredContent"]["page"]["returned"], 1);
    }

    #[test]
    fn mcp_config_controls_advertised_and_runtime_defaults() {
        let (dir, db) = fixture();
        let mut config = config_for_fixture(&dir);
        config.mcp.search_sessions_limit = 7;
        config.mcp.list_sessions_limit = 8;
        config.mcp.search_messages_limit = 1;
        config.mcp.get_session_transcript_lines = -3;
        config.mcp.preview_chars = 10;

        let v = handle_tools_list(Some(json!(1)), &config);
        let tools = v["result"]["tools"].as_array().unwrap();
        let search_sessions = tools
            .iter()
            .find(|t| t["name"] == "search_sessions")
            .expect("search_sessions advertised");
        let list_sessions = tools
            .iter()
            .find(|t| t["name"] == "list_sessions")
            .expect("list_sessions advertised");
        let get_session = tools
            .iter()
            .find(|t| t["name"] == "get_session")
            .expect("get_session advertised");
        let search_messages = tools
            .iter()
            .find(|t| t["name"] == "search_messages")
            .expect("search_messages advertised");

        assert_eq!(
            search_sessions["inputSchema"]["properties"]["limit"]["default"],
            7
        );
        assert_eq!(
            list_sessions["inputSchema"]["properties"]["limit"]["default"],
            8
        );
        assert_eq!(
            get_session["inputSchema"]["properties"]["transcript_lines"]["default"],
            -3
        );
        assert_eq!(
            get_session["inputSchema"]["properties"]["preview_chars"]["default"],
            10
        );
        assert_eq!(
            search_messages["inputSchema"]["properties"]["limit"]["default"],
            1
        );
        assert_eq!(
            search_messages["inputSchema"]["properties"]["field_view"]["default"],
            json!({"kind": "max_chars", "max_chars": 10})
        );
        assert!(get_session["description"]
            .as_str()
            .is_some_and(|d| d.contains("last 3 transcript lines")));

        let page = search_messages_value(&json!({ "query": "hello" }), &config, &db);
        assert_eq!(page["page"]["returned"], 1);
        assert_eq!(page["page"]["next_offset"], 1);
        assert_eq!(
            page["effective_request"]["presentation"]["field_view"],
            json!({"kind": "max_chars", "max_chars": 10})
        );
        assert_eq!(
            page["results"][0]["presentation"]["field_view"]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count(),
            10
        );
        assert_eq!(
            page["results"][0]["presentation"]["field_view"]["additional_field_text"],
            "after"
        );

        let session =
            tool_get_session(&json!({ "session_id": "claude:test1" }), &config, &db).unwrap();
        assert!(session.contains("- Transcript lines returned: last 3"));
        assert!(!session.contains("transcript line 401"));
        assert!(session.contains("transcript line 402"));
    }

    #[test]
    fn search_messages_schema_projects_the_planners_configured_mcp_defaults() {
        let (dir, _db) = fixture();
        let mut config = config_for_fixture(&dir);
        config.mcp.search_messages_limit = 3;
        config.mcp.preview_chars = 41;
        config.mcp.lines_per_message = -7;
        config.search.message_search.context.context_before = Some(2);
        config.search.message_search.context.context_after = Some(5);
        config.search.message_search.match_evidence_max_chars = std::num::NonZeroUsize::new(17);

        let configured = serde_json::to_value(
            MessageService::message_search_spec_for_config(
                &config,
                crate::message_search::SearchSurface::Mcp,
            )
            .expect("validated MCP config resolves")
            .configured_default(),
        )
        .expect("configured request serializes");
        let schema = tool_input_schema(&config, "search_messages")["inputSchema"].clone();
        let properties = schema["properties"]
            .as_object()
            .expect("search_messages properties");

        assert_eq!(
            properties["limit"]["default"],
            configured["extent"]["limit"]
        );
        assert_eq!(
            properties["offset"]["default"],
            configured["extent"]["offset"]
        );
        assert_eq!(
            properties["context_before"]["default"],
            configured["context"]["messages_before"]
        );
        assert_eq!(
            properties["context_after"]["default"],
            configured["context"]["messages_after"]
        );
        assert!(
            properties["context"].get("default").is_none(),
            "an asymmetric configured context cannot be represented as one radius"
        );
        assert_eq!(
            properties["lines_per_message"]["default"],
            configured["presentation"]["lines_per_message"]
        );
        assert_eq!(
            properties["field_view"]["default"],
            configured["presentation"]["field_view"]
        );
        assert_eq!(
            properties["match_view"]["default"],
            configured["presentation"]["match_view"]
        );
        assert_eq!(properties["include"]["default"], configured["include"]);
        assert_eq!(
            properties["receipt_level"]["default"],
            configured["receipt_level"]
        );
    }

    #[test]
    fn search_messages_schema_covers_every_canonical_parameter_and_mcp_property() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let schema = tool_input_schema(&config, "search_messages")["inputSchema"].clone();
        let properties = schema["properties"]
            .as_object()
            .expect("search_messages properties");
        let registry = crate::message_search::MessageSearchParameterRegistry::current();
        let canonical_names = registry
            .parameters()
            .iter()
            .map(|parameter| parameter.parameter().as_str())
            .collect::<std::collections::BTreeSet<_>>();

        // The mapping this test proves used to be read back off the wire from
        // `x-aise-parameters`. No MCP client reads that key, so the join now runs against
        // `message_search_mcp_fields` directly. That is the stronger check: the function is
        // exhaustive over `MessageSearchParameter`, so a new canonical parameter cannot compile
        // until it names its wire fields, where a missing vendor key only failed at test time.

        // Forward: every canonical parameter projects onto fields the schema actually advertises.
        let mut projected = std::collections::BTreeSet::new();
        for parameter in registry.parameters() {
            let fields = message_search_mcp_fields(parameter.parameter());
            assert!(
                !fields.is_empty(),
                "{} has no MCP projection",
                parameter.parameter().as_str()
            );
            for field in fields {
                assert!(
                    properties.contains_key(*field),
                    "canonical parameter {} projects onto {field}, which the schema does not \
                     advertise",
                    parameter.parameter().as_str()
                );
                projected.insert(*field);
            }
            assert!(
                canonical_names.contains(parameter.parameter().as_str()),
                "{} is projected but is not in the registry",
                parameter.parameter().as_str()
            );
        }

        // Reverse: nothing is advertised that no planner parameter owns. An accepted-but-unowned
        // field is the contract failure `REQ044-automate-safe-problem-solving` names -- it reads
        // as supported and reaches no behaviour.
        projected.insert(ADAPTER_CONTROL_FIELD);
        let advertised = properties
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            advertised,
            projected,
            "advertised fields and planner-owned fields disagree; unowned: {:?}, unadvertised: {:?}",
            advertised.difference(&projected).collect::<Vec<_>>(),
            projected.difference(&advertised).collect::<Vec<_>>()
        );

        // The nine conflict rules used to ship as `x-aise-specification`, which measured zero
        // readers: no client is specified to read a vendor key and no model was ever shown one.
        // `WP-N-generate-message-search-descriptions` renders them verbatim into
        // `tool.description`, the one channel every client forwards and Codex never strips.
        assert!(
            !serde_json::to_string(&schema)
                .expect("schema serializes")
                .contains("x-aise-"),
            "an x-aise-* key is back on the wire"
        );
        assert_eq!(
            registry.rules().len(),
            9,
            "the conflict-rule count changed; tool.description renders them verbatim and its \
             2,048-character ceiling is budgeted against this count"
        );
    }

    /// Every field the parser accepts is advertised, and nothing else is.
    ///
    /// `additionalProperties: false` makes this a correctness requirement rather than a courtesy.
    /// A validating client rejects an argument the schema does not declare before the request
    /// ever reaches this server, so a field accepted but unadvertised is a field that cannot be
    /// used from those clients -- and one advertised but unaccepted is a promise the parser
    /// breaks. Both directions have to hold at once.
    #[test]
    fn search_messages_advertises_exactly_the_fields_it_accepts() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let schema = tool_input_schema(&config, "search_messages")["inputSchema"].clone();
        assert_eq!(
            schema["additionalProperties"],
            json!(false),
            "the schema is open, so this test cannot say anything about what is rejected"
        );

        let advertised = schema["properties"]
            .as_object()
            .expect("search_messages properties")
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let accepted = MESSAGE_SEARCH_MCP_FIELDS
            .iter()
            .map(|(field, _, _)| *field)
            .chain(std::iter::once(ADAPTER_CONTROL_FIELD))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            advertised,
            accepted,
            "advertised but not accepted: {:?}; accepted but not advertised: {:?}",
            advertised.difference(&accepted).collect::<Vec<_>>(),
            accepted.difference(&advertised).collect::<Vec<_>>()
        );
        assert_eq!(advertised.len(), 37, "{advertised:?}");

        // `offset` is the field an earlier design proposed hiding. It cannot be hidden: the
        // response tells callers to send it back, so concealing it would publish an instruction
        // a validating client is unable to follow.
        assert!(advertised.contains("offset"));
    }

    /// The paging instruction a response gives is one a validating client can actually follow.
    #[test]
    fn pagination_follow_up_uses_an_advertised_argument() {
        // The fixture carries two `hello` messages, so one page of one leaves a second.
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let first = search_messages_value(&json!({ "query": "hello", "limit": 1 }), &config, &db);
        let next = first["page"]["next_offset"]
            .as_u64()
            .expect("a truncated page states the offset to continue from");
        assert_eq!(first["page"]["has_more"], json!(true));

        let advertised = tool_input_schema(&config, "search_messages")["inputSchema"]["properties"]
            .as_object()
            .expect("properties")
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            advertised.contains("offset") && advertised.contains("limit"),
            "the response names arguments the schema does not advertise: {advertised:?}"
        );

        let second = search_messages_value(
            &json!({ "query": "hello", "limit": 1, "offset": next }),
            &config,
            &db,
        );
        assert_ne!(
            first["results"], second["results"],
            "following next_offset returned the same page"
        );
    }

    /// Naming the shared character-budget variant must not change what either view accepts.
    ///
    /// The two views look like one shape and are not: their bounded variants are byte-identical,
    /// their unbounded ones are `no_char_limit` and `minimal_span`. Extracting the whole view
    /// saves more bytes and silently makes `match_view` accept `no_char_limit` while rejecting
    /// `minimal_span`. That is the exact regression a cold read caught this work about to make
    /// by hand, so it is pinned rather than commented.
    #[test]
    fn extracting_the_shared_char_budget_keeps_each_view_s_own_variants() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let schema = tool_input_schema(&config, "search_messages")["inputSchema"].clone();

        let shared = &schema["$defs"][CHAR_BUDGET_DEF];
        assert!(
            shared.is_object(),
            "the shared variant was not extracted: {schema}"
        );
        // A single-value enum, not a const: Codex models enum and drops const, so the
        // discriminator only reaches the model in this form.
        assert_eq!(shared["properties"]["kind"]["enum"], json!(["max_chars"]));

        for (view, own) in [
            ("field_view", "no_char_limit"),
            ("match_view", "minimal_span"),
        ] {
            let variants = schema["properties"][view]["oneOf"]
                .as_array()
                .unwrap_or_else(|| panic!("{view} declares its variants"));
            assert_eq!(variants.len(), 2, "{view}: {variants:?}");
            let tags = variants
                .iter()
                .filter_map(|variant| variant["properties"]["kind"]["enum"][0].as_str())
                .collect::<Vec<_>>();
            assert_eq!(
                tags,
                vec![own],
                "{view} lost its own unbounded variant: {variants:?}"
            );
            assert!(
                variants
                    .iter()
                    .any(|variant| variant["$ref"] == json!(format!("#/$defs/{CHAR_BUDGET_DEF}"))),
                "{view} does not reference the shared bounded variant: {variants:?}"
            );
        }

        // A ref a client must not dereference is a ref it cannot resolve, and an unreachable
        // definition is bytes Codex prunes before it measures.
        assert!(crate::mcp_schema_budget::collect_refs(&schema)
            .iter()
            .all(|target| target.starts_with("#/$defs/")));
        assert!(crate::mcp_schema_budget::unreachable_definitions(&schema).is_empty());
    }

    /// One concept renders one description, and two concepts sharing a spelling do not.
    ///
    /// Assertion (b) is the load-bearing one and the reason this test exists at all. The whole
    /// risk of a shared-description table is that keying it on the string `since` publishes one
    /// sentence that is wrong on two of the four tools carrying that name: it bounds session
    /// activity on `list_sessions` and `search_sessions`, and one message's own timestamp on
    /// `search_messages` and `run_skill_capability`. A future pass that "deduplicates" the two
    /// into one fails here rather than shipping.
    #[test]
    fn one_concept_renders_one_description_and_two_concepts_stay_apart() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let listed = handle_tools_list(Some(json!(1)), &config);
        let tools = listed["result"]["tools"].as_array().expect("tools list");
        let described = |tool_name: &str, field: &str| -> Option<String> {
            let tool = tools.iter().find(|tool| tool["name"] == tool_name)?;
            let mut schema = &tool["inputSchema"]["properties"];
            let segments: Vec<&str> = field.split('.').collect();
            for (index, segment) in segments.iter().enumerate() {
                schema = &schema[segment];
                if index + 1 < segments.len() {
                    schema = &schema["properties"];
                }
            }
            schema["description"].as_str().map(str::to_owned)
        };

        // (a) Every field mapped to one concept renders byte-identical text.
        for concept_row in TOOL_FIELD_CONCEPT {
            let (tool, field, concept) = concept_row;
            let rendered = described(tool, field)
                .unwrap_or_else(|| panic!("{tool}.{field} has no description"));
            assert_eq!(
                rendered,
                concept_description(*concept),
                "{tool}.{field} does not render its concept's text"
            );
        }

        // (b) The two time concepts that share the `since` spelling render different text.
        let session_activity = concept_description(ToolParameterConcept::SessionActivityWindow);
        let message_timestamp = concept_description(ToolParameterConcept::MessageTimestampWindow);
        assert_ne!(
            session_activity, message_timestamp,
            "the two `since` concepts collapsed into one description, which is wrong on two of \
             the four tools that spell the parameter `since`"
        );
        assert_eq!(
            described("list_sessions", "since").as_deref(),
            Some(session_activity)
        );
        assert_eq!(
            described("run_skill_capability", "since").as_deref(),
            Some(message_timestamp)
        );
        assert_ne!(
            concept_description(ToolParameterConcept::SessionActivityPeriod),
            concept_description(ToolParameterConcept::MessageTimestampPeriod),
            "the two `when` concepts collapsed the same way"
        );

        // (c) Every mapped pair names a field that exists, so a rename leaves no dead row.
        for (tool, field, _) in TOOL_FIELD_CONCEPT {
            assert!(
                described(tool, field).is_some(),
                "{tool}.{field} is mapped to a concept but is not advertised"
            );
        }

        // `search_messages` is absent throughout: its registry owns every description it has,
        // and a second writer would put two owners on one string again.
        assert!(
            TOOL_FIELD_CONCEPT
                .iter()
                .all(|(tool, _, _)| *tool != "search_messages"),
            "the concept table writes over registry-generated search_messages text"
        );

        // The one advertised parameter that had no description at all now has one.
        let categories = described("run_skill_capability", "definition.categories")
            .expect("definition.categories is advertised");
        assert!(!categories.trim().is_empty(), "{categories:?}");
    }

    /// A deterministic mixed-session corpus, built so a default search has something to decide
    /// between: the same query text in four sessions across two repositories, one of them a run
    /// spawned by another, with different recency.
    ///
    /// It replaces a capture taken from a real index. That capture measured the figures this work
    /// was planned against, and it could not be committed or regenerated: it carried real
    /// repository paths, so it was untracked, and an untracked fixture is a measurement nobody
    /// else can reproduce.
    fn mixed_session_fixture() -> (tempfile::TempDir, Db) {
        let (dir, db) = fixture();
        insert_subagent_session(&db, "claude:spawned", "claude:test1", "explore");
        insert_list_session(&db, "claude:older", "2026-01-02T00:00:00Z");
        insert_list_session(&db, "claude:newer", "2026-06-30T00:00:00Z");
        (dir, db)
    }

    /// The response-artifact rows, measured against a fixture rather than left unmeasured.
    ///
    /// `evaluate` skips these because a catalogue cannot observe a `tools/call` result, and the
    /// report says so rather than reporting them as passing. This is where they are actually
    /// checked, and it is the whole reason the ceiling in `WP-F` has a number to defend.
    #[test]
    fn a_default_search_result_fits_every_client_result_cap() {
        use crate::mcp_schema_budget::{evaluate_response, Status};

        let (dir, db) = mixed_session_fixture();
        let config = config_for_fixture(&dir);
        let response = search_messages_value(&json!({ "query": "hello" }), &config, &db);
        let result = tool_response_to_rmcp(ToolResponse::structured(response).expect("serializes"));
        let measured = serde_json::to_string(&result)
            .expect("result serializes")
            .chars()
            .count();

        let findings = evaluate_response("search_messages", measured);
        assert_eq!(findings.len(), 3, "three clients declare a result cap");
        let breached: Vec<String> = findings
            .iter()
            .filter(|finding| finding.status != Status::Pass)
            .map(|finding| {
                format!(
                    "{}: {} {} against {}",
                    finding.limit.name, finding.measured, finding.limit.unit, finding.limit.budget
                )
            })
            .collect();
        assert!(
            breached.is_empty(),
            "a default search result breaches a client cap: {breached:?}"
        );

        // The tightest cap that fails silently is the one the configured ceiling defends, so a
        // default call has to sit well inside it rather than merely under it.
        assert!(
            measured * 2 < config.mcp.max_tool_result_chars,
            "a default result is {measured} characters, more than half the \
             {} ceiling, leaving no room for a long matched message",
            config.mcp.max_tool_result_chars
        );
    }

    /// The default response carries what an agent needs to choose between arbitrary sessions.
    ///
    /// This is the requirement the metadata trim would have broken: a plain query has to be
    /// enough to tell two repositories apart, tell a spawned run from its root, and name the
    /// exact follow-up. No preset, no extra include, no configuration.
    #[test]
    fn a_plain_query_returns_enough_to_choose_between_sessions() {
        let (dir, db) = mixed_session_fixture();
        let config = config_for_fixture(&dir);
        let response = search_messages_value(&json!({ "query": "hello" }), &config, &db);

        let metadata = response["included"]["normalized_session_metadata"]
            .as_object()
            .expect("an omitted include still returns deduplicated session metadata");
        assert!(
            metadata.len() >= 2,
            "the fixture spans several sessions but only {} reached the response",
            metadata.len()
        );
        for (session, facts) in metadata {
            for field in ["cwd", "repo_root", "title", "updated_at", "message_count"] {
                assert!(
                    facts.get(field).is_some(),
                    "{session} is missing {field}, which is what distinguishes it from another \
                     session carrying the same matched text"
                );
            }
        }
        assert!(
            metadata
                .values()
                .any(|facts| facts["parent_session_id"].is_string()),
            "a spawned run is indistinguishable from a root session: {metadata:?}"
        );

        // Every hit names the exact expansion call, so choosing is followed by retrieving.
        for hit in response["results"].as_array().expect("results") {
            assert!(hit["message_ref"]["session_id"].is_string(), "{hit}");
            assert!(hit["message_ref"]["message_seq"].is_number(), "{hit}");
        }
        assert!(response["page"]["has_more"].is_boolean());
    }

    /// An oversized result is a visible error, and no explicit payload is quietly removed.
    ///
    /// The alternative -- trimming until it fits -- is what `REQ008-reject-hidden-cutoffs`
    /// forbids by name, and what Codex already does to us: it middle-truncates with no marker,
    /// keeping a plausible head and tail, so a caller cannot tell a complete answer from a
    /// mutilated one. Erroring is worse ergonomics and strictly better behaviour.
    #[test]
    fn an_oversized_result_errors_rather_than_arriving_partially() {
        let mut full = rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
            "x".repeat(4_000),
        )]);
        // Message search nests its paging under `page`, and the advice quotes those exact paths,
        // so the fixture carries the block rather than leaving the shape to be guessed at.
        full.structured_content = Some(json!({
            "page": { "returned": 15, "has_more": true, "next_offset": 15 },
            "results": (0..15)
                .map(|seq| json!({ "message_seq": seq, "text": "y".repeat(200) }))
                .collect::<Vec<Value>>(),
        }));
        let measured = serde_json::to_string(&full).unwrap().chars().count();

        let paged = ToolRecovery {
            paging: ToolPaging::Page(15),
            reducers: ReducingArguments::default(),
        };

        // Under the ceiling the result is delivered untouched, byte for byte.
        let delivered =
            enforce_tool_result_ceiling(full.clone(), "search_messages", measured + 1, paged)
                .expect("a result inside the ceiling is delivered");
        assert_eq!(
            serde_json::to_string(&delivered).unwrap(),
            serde_json::to_string(&full).unwrap(),
            "a result inside the ceiling must not be reshaped on the way out"
        );

        // Over it, nothing is delivered at all.
        let error = enforce_tool_result_ceiling(full, "search_messages", measured - 1, paged)
            .expect_err("a result over the ceiling must not be delivered");
        for required in [
            "search_messages produced",
            "above the configured MCP ceiling",
            "Nothing was returned",
            "silently removed",
            "same offset",
            "max_tool_result_chars",
            "AI_SESSION_SEARCH_MAX_TOOL_RESULT_CHARS",
            "page.next_offset",
        ] {
            assert!(
                error.contains(required),
                "missing {required:?} from: {error}"
            );
        }
        assert!(
            error.contains(&measured.to_string()),
            "the error must state what it measured: {error}"
        );
    }

    /// An error result is delivered whatever its size, because the alternative is silence.
    #[test]
    fn the_ceiling_never_suppresses_an_error_result() {
        let error = rmcp_tool_error("y".repeat(4_000));
        let delivered = enforce_tool_result_ceiling(
            error,
            "search_messages",
            10,
            ToolRecovery {
                paging: ToolPaging::Page(15),
                reducers: ReducingArguments::default(),
            },
        )
        .expect("an error result is delivered regardless of size");
        assert_eq!(delivered.is_error, Some(true));
    }

    /// The suggested retry is always smaller than the page that actually overflowed.
    ///
    /// A suggestion that errors again costs a round trip and teaches the caller that the advice
    /// is unreliable. The sharper failure is a suggestion that is *larger*: scaling from a fixed
    /// page instead of the requested one told a `limit=1` caller to retry with `limit=12`, which
    /// is more work than the call that just failed, offered as its remedy.
    #[test]
    fn the_suggested_retry_limit_is_always_smaller_than_the_page_that_overflowed() {
        for (measured, ceiling) in [
            (48_001usize, 48_000usize),
            (96_000, 48_000),
            (480_000, 48_000),
            (4_800_000, 48_000),
        ] {
            for resolved_page in [2usize, 3, 15, 20, 50, 500] {
                // A response that is nearly all results, evenly sized, so the envelope is not
                // what blocks a retry. Whether any page fits then depends on what one result
                // costs: at 4,800,000 characters over two results, one alone is fifty times the
                // ceiling, and the honest answer is that no page size helps.
                // Sized so the parts are consistent: the results sum to exactly the scaling
                // portion, so each result costs what it says and nothing rides along with it.
                let per_result = ((measured - 200) / resolved_page).max(1);
                let even = ResponseShape {
                    scaling_chars: per_result * resolved_page,
                    envelope_chars: 200,
                    result_sizes: vec![per_result; resolved_page],
                };
                match suggested_retry_limit(measured, ceiling, resolved_page, Some(&even)) {
                    Some(suggested) => {
                        assert!(
                            (1..resolved_page).contains(&suggested),
                            "suggested {suggested} is not a smaller usable page than \
                             {resolved_page} for {measured}/{ceiling}"
                        );
                        assert!(
                            even.envelope_chars + suggested * per_result <= ceiling,
                            "suggested {suggested} projects over the {ceiling} ceiling"
                        );
                    }
                    None => assert!(
                        even.envelope_chars + per_result > ceiling,
                        "a page of one fits at {per_result} per result under {ceiling}, so \
                         refusing to name one withholds advice the caller could have used"
                    ),
                }

                // No structured content to decompose still has to produce usable advice.
                let blind = suggested_retry_limit(measured, ceiling, resolved_page, None)
                    .expect("a page above 1 can always be reduced");
                assert!((1..resolved_page).contains(&blind), "blind {blind}");

                // An envelope over the ceiling is the case no page size can solve, and saying so
                // is the only answer that does not waste the caller's next call.
                assert_eq!(
                    suggested_retry_limit(
                        measured,
                        ceiling,
                        resolved_page,
                        Some(&ResponseShape {
                            scaling_chars: measured - ceiling - 1,
                            envelope_chars: ceiling + 1,
                            result_sizes: vec![
                                (measured - ceiling - 1).div_ceil(resolved_page);
                                resolved_page
                            ],
                        }),
                    ),
                    None,
                    "an envelope larger than the ceiling cannot be paged away"
                );
            }

            assert_eq!(
                suggested_retry_limit(measured, ceiling, 1, None),
                None,
                "one result already exceeds the ceiling, so there is no smaller page to name \
                 and inventing one sends the caller to a call that cannot succeed"
            );
        }
    }

    /// The suggestion is one an installed server can actually honor.
    ///
    /// Every figure here was measured against a real index through `mcp serve`, because that is
    /// where each wrong answer this function has given was found. Two shipped and one was caught
    /// before shipping, and all three are pinned so none can return.
    ///
    /// A `limit=20` search measured 30,518 characters against a 6,000-character ceiling and its
    /// `limit=3` prefix measured 6,358, so 3 does not fit and 2, at 4,732, does.
    ///
    /// The first answer was 12, from scaling a hard-coded 15 that ignored the request entirely.
    /// The second was 3, from scaling `page * ceiling / measured`, which ignores the fixed
    /// envelope. The third was also 3, from counting `included` as shrinking with the page: it
    /// holds one entry per distinct session, and this page and its 3-result prefix cover the same
    /// two sessions, so 1,021 characters that never shrink were being promised away.
    #[test]
    fn the_suggestion_is_one_an_installed_server_can_honor() {
        let ceiling = 6_000;
        let page = 20;
        // Twenty near-uniform results, as measured: 1,130 characters each.
        let result_sizes = vec![1_130usize; page];
        let results_only: usize = result_sizes.iter().sum();
        let rendered = 4_729;
        let shape = ResponseShape {
            // Only the results and their text rendering shrink with the page.
            scaling_chars: results_only + rendered,
            // The echoed request, the page block, the wrapper, and the per-session metadata that
            // a shorter page does not shed.
            envelope_chars: 814 + 1_021,
            result_sizes,
        };
        let measured = shape.scaling_chars + shape.envelope_chars;

        let suggested = suggested_retry_limit(measured, ceiling, page, Some(&shape))
            .expect("a page fits under this ceiling");

        assert_eq!(
            suggested, 2,
            "the installed server measured limit=3 at 6,358 characters against this 6,000 \
             ceiling and limit=2 at 4,732, so 2 is the largest page a caller can be sent to"
        );
        assert!(
            suggested < page,
            "the suggestion must be smaller than the {page} that overflowed"
        );

        // Pricing by the page mean is what answered 3 twice. Assert it still would, so this test
        // is demonstrably reproducing the defect rather than passing for an unrelated reason.
        let mean_priced = (ceiling - shape.envelope_chars)
            / ((results_only + rendered) as f64 / page as f64).ceil() as usize;
        assert_eq!(
            mean_priced, 3,
            "the page mean answers 3 on this shape, and 3 measured over the ceiling"
        );
    }

    /// The recovery text names only arguments the failed tool actually accepts.
    ///
    /// Three of the eight tools return one whole answer rather than a page. Telling their caller
    /// to lower `limit` and continue from `page.next_offset` names two arguments the schema
    /// rejects and one response field the tool never returns, so following the advice exactly
    /// produces a validation error rather than a smaller result.
    #[test]
    fn overflow_recovery_names_no_argument_the_failed_tool_rejects() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let advertised = advertised_tools(&config);
        let oversized =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                "x".repeat(4_000),
            )]);

        for pageless in ["get_session", "get_resume_command", "get_index_status"] {
            let recovery = ToolRecovery::for_call(pageless, &json!({}), &advertised);
            assert_eq!(
                recovery.paging,
                ToolPaging::Unpaged,
                "{pageless} does not advertise limit, so it has no page to shrink"
            );
            let error =
                enforce_tool_result_ceiling(oversized.clone(), pageless, 10, recovery).unwrap_err();
            for invalid in ["limit=", "page.next_offset", "offset"] {
                assert!(
                    !error.contains(invalid),
                    "{pageless} cannot act on {invalid:?}: {error}"
                );
            }
            assert!(
                error.contains("max_tool_result_chars"),
                "a caller with no page to shrink still needs the one remedy that applies: {error}"
            );
        }

        for pageable in [
            "search_messages",
            "search_sessions",
            "list_sessions",
            "query_session_index",
            "run_skill_capability",
        ] {
            let recovery = ToolRecovery::for_call(pageable, &json!({ "limit": 20 }), &advertised);
            assert_eq!(recovery.paging, ToolPaging::Page(20));
            let error =
                enforce_tool_result_ceiling(oversized.clone(), pageable, 10, recovery).unwrap_err();
            assert!(
                error.contains("limit="),
                "{pageable} pages, so the smaller page is the first thing to name: {error}"
            );
        }

        // Every argument the advice names has to exist on the tool it is advising about, not just
        // on the tool the advice was written for. `search_sessions` pages but accepts no
        // `context`, `detail` or `receipt_level`, so naming those sends its caller to three
        // arguments its schema rejects -- the same defect as above, on the other branch.
        for tool in advertised.as_array().expect("advertised tools") {
            let name = tool["name"].as_str().expect("tool name");
            let properties = tool["inputSchema"]["properties"]
                .as_object()
                .map(|properties| {
                    properties
                        .keys()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>()
                })
                .unwrap_or_default();
            let recovery = ToolRecovery::for_call(name, &json!({}), &advertised);
            let error =
                enforce_tool_result_ceiling(oversized.clone(), name, 10, recovery).unwrap_err();
            for argument in [
                "limit",
                "offset",
                "context",
                "detail",
                "receipt_level",
                "lines_per_message",
                "field_view",
            ] {
                if properties.contains(argument) {
                    continue;
                }
                assert!(
                    !error.contains(argument),
                    "{name} does not accept {argument:?}, so the advice cannot name it: {error}"
                );
            }
        }
    }

    /// An omitted `limit` still resolves to the configured page, and that is what shrinks.
    ///
    /// Reading the resolved page from the advertised schema keeps one owner: the same `default`
    /// the model is shown is the number the retry advice is computed from, so the two cannot
    /// describe different pages.
    #[test]
    fn the_recovery_page_comes_from_the_schema_the_caller_read() {
        let (dir, _db) = fixture();
        let mut config = config_for_fixture(&dir);
        config.mcp.search_messages_limit = 9;
        let advertised = advertised_tools(&config);

        assert_eq!(
            ToolRecovery::for_call("search_messages", &json!({}), &advertised).paging,
            ToolPaging::Page(9),
            "an omitted limit resolves to the configured page, which is what a retry must beat"
        );
        assert_eq!(
            ToolRecovery::for_call("search_messages", &json!({ "limit": 3 }), &advertised).paging,
            ToolPaging::Page(3),
            "an explicit limit is the page that overflowed"
        );
        assert_eq!(
            ToolRecovery::for_call("search_sessions", &json!({ "limit": 0 }), &advertised).paging,
            ToolPaging::EveryMatch,
            "limit=0 asks the session tools for every match, which is unbounded rather than absent"
        );
    }

    /// A caller who asked for every match is told which finite page fits, not that none exists.
    ///
    /// `limit=0` means "every match" on the session tools, and an earlier reading collapsed that
    /// into the same state as a tool advertising no `limit` at all. The advice that follows says
    /// there is no smaller page to ask for -- on a tool whose own schema advertises `limit` with a
    /// default of 10. An installed Codex run met exactly this: `search_sessions(limit=0)` produced
    /// 154,174 characters against the 48,000 ceiling, was told no smaller page existed, and then
    /// answered the same question with `limit=20`. The advice was not merely imprecise; it denied
    /// the remedy the caller went on to use.
    ///
    /// Two facts make the finite page computable, and both are already in the response: the count
    /// it returned, and the items themselves. The session tools report that count at the top level
    /// as `returned` and carry the items under `sessions`, where message search reports
    /// `page.returned` and carries `results`, so neither may be read from one fixed path.
    #[test]
    fn an_unbounded_page_that_overflows_is_told_which_finite_page_fits() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let advertised = advertised_tools(&config);

        // Forty sessions of roughly 500 characters each, the shape `search_sessions(limit=0)`
        // returns: the count at the top level, the items beside it, and no `page` object at all.
        let sessions: Vec<Value> = (0..40)
            .map(|index| {
                json!({
                    "id": format!("claude:{index:032}"),
                    "title": "x".repeat(400),
                    "repo_root": "/Users/athundt/source/rtk",
                })
            })
            .collect();
        let mut oversized =
            rmcp::model::CallToolResult::success(vec![rmcp::model::ContentBlock::text(
                "rendered listing".to_owned(),
            )]);
        oversized.structured_content = Some(json!({
            "returned": 40,
            "has_more": false,
            "next_offset": Value::Null,
            "sessions": sessions,
        }));
        let measured = serde_json::to_string(&oversized).unwrap().chars().count();
        let ceiling = measured / 4;

        let recovery =
            ToolRecovery::for_call("search_sessions", &json!({ "limit": 0 }), &advertised);
        let error = enforce_tool_result_ceiling(oversized, "search_sessions", ceiling, recovery)
            .expect_err("a result over the ceiling must not be delivered");

        assert!(
            !error.contains("no smaller page"),
            "search_sessions advertises limit and offset, so a finite page exists: {error}"
        );
        let suggested = error
            .split("limit=")
            .nth(1)
            .and_then(|tail| {
                tail.split(|character: char| !character.is_ascii_digit())
                    .next()
                    .filter(|digits| !digits.is_empty())
            })
            .and_then(|digits| digits.parse::<usize>().ok())
            .unwrap_or_else(|| panic!("the advice must name a finite page: {error}"));
        assert!(
            (1..40).contains(&suggested),
            "a retry of {suggested} is not smaller than the 40 sessions that overflowed: {error}"
        );
        // The suggestion has to be arithmetically sound, not merely present: a quarter of the
        // response fits, so a page near a quarter of the items does too.
        let per_session = measured / 40;
        assert!(
            suggested * per_session <= ceiling,
            "a page of {suggested} at about {per_session} characters each still exceeds {ceiling}"
        );
    }

    /// The suggested retry actually fits, executed rather than asserted.
    ///
    /// The estimate is advice, and advice that fails is worse than none. This runs the real
    /// search at the suggested page against the same fixture and requires the result to be
    /// deliverable under the same ceiling that rejected the original.
    #[test]
    fn the_suggested_retry_fits_when_it_is_executed() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let advertised = advertised_tools(&config);

        let requested = 8usize;
        let arguments = json!({ "query": "hello", "limit": requested });
        let full = tool_response_to_rmcp(
            ToolResponse::structured(search_messages_value(&arguments, &config, &db))
                .expect("response serializes"),
        );
        let measured = serde_json::to_string(&full).unwrap().chars().count();

        // A ceiling one character under the measured size is the hardest case for an estimate:
        // the smallest possible overflow is where an optimistic guess most easily lands back
        // over the line.
        let ceiling = measured - 1;
        let recovery = ToolRecovery::for_call("search_messages", &arguments, &advertised);
        let error = enforce_tool_result_ceiling(full, "search_messages", ceiling, recovery)
            .expect_err("the result is over the ceiling");

        // Read the suggestion out of the text the caller actually receives, rather than
        // recomputing it here. A test that recomputes the advice can pass while the sentence
        // shipped to the caller says something else.
        let suggested: usize = error
            .split("limit=")
            .nth(1)
            .expect("the advice names a smaller page")
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .expect("the named page is a number")
            .parse()
            .expect("the named page is a number");
        assert!(
            suggested < requested,
            "the advice must name a smaller page than the {requested} that overflowed: {error}"
        );

        let retried = tool_response_to_rmcp(
            ToolResponse::structured(search_messages_value(
                &json!({ "query": "hello", "limit": suggested }),
                &config,
                &db,
            ))
            .expect("response serializes"),
        );
        let retried_size = serde_json::to_string(&retried).unwrap().chars().count();
        assert!(
            enforce_tool_result_ceiling(retried, "search_messages", ceiling, recovery).is_ok(),
            "the retry this error tells the caller to make must succeed: it named \
             limit={suggested} against ceiling {ceiling} after {measured} overflowed, and that \
             retry measured {retried_size}"
        );
    }

    /// The shipped default page fits with headroom, and the ceiling is the backstop it is meant
    /// to be rather than the mechanism.
    #[test]
    fn the_default_page_fits_well_inside_the_configured_ceiling() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        assert_eq!(
            config.mcp.search_messages_limit, 15,
            "the shipped MCP page size is what the ceiling was sized against"
        );

        let response = search_messages_value(&json!({ "query": "hello" }), &config, &db);
        let result =
            tool_response_to_rmcp(ToolResponse::structured(response).expect("response serializes"));
        let measured = serde_json::to_string(&result).unwrap().chars().count();
        let ceiling = config.mcp.max_tool_result_chars;
        assert_eq!(ceiling, 48_000);
        assert!(
            measured < ceiling,
            "the default call is already at the ceiling: {measured} of {ceiling}"
        );
        assert!(
            enforce_tool_result_ceiling(
                result,
                "search_messages",
                ceiling,
                ToolRecovery::for_call("search_messages", &json!({}), &advertised_tools(&config)),
            )
            .is_ok(),
            "an ordinary default call must never take the error path"
        );
    }

    /// Every validator rule reaches the model verbatim, in the one channel no client strips.
    ///
    /// Substring containment rather than equality, because the rendered block adds a heading and
    /// indentation. Paraphrasing any message fails this, which is the drift that matters: the
    /// moment the published text is authored separately from the validator's, the published
    /// rules stop describing behaviour. Generation is what makes that unrepresentable; this
    /// asserts it rather than assuming it.
    #[test]
    fn search_messages_description_publishes_every_conflict_rule_verbatim() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let listed = handle_tools_list(Some(json!(1)), &config);
        let description = listed["result"]["tools"]
            .as_array()
            .expect("tools list")
            .iter()
            .find(|tool| tool["name"] == "search_messages")
            .expect("search_messages is served")["description"]
            .as_str()
            .expect("tool description is text")
            .to_owned();

        let registry = crate::message_search::MessageSearchParameterRegistry::current();
        let descriptors = registry.rule_descriptors();
        assert!(
            !descriptors.is_empty(),
            "the validator declares no rules to publish"
        );
        for descriptor in descriptors {
            assert!(
                description.contains(descriptor.message()),
                "conflict rule {} is missing or paraphrased in tool.description.\n  wanted: {}\n  \
                 got: {description}",
                descriptor.rule().as_str(),
                descriptor.message()
            );
        }

        // Every rule reaching the model is worth nothing if the channel truncates first. This
        // block is the part that grows without anyone editing a description, so it is the part
        // that pushes the channel over Claude Code's silent cut.
        let measured = description.chars().count();
        assert!(
            measured <= 2_048,
            "tool.description is {measured} chars; Claude Code truncates at 2,048 and appends a \
             marker the model never sees as a loss"
        );
    }

    /// The two field tables describe the same set, in both directions.
    ///
    /// `message_search_mcp_fields` is a match over the parameter enum, so the compiler proves it
    /// covers every canonical parameter. `MESSAGE_SEARCH_MCP_FIELDS` adds the wire role each
    /// spelling plays. Neither can prove the other complete, so a field added to one and
    /// forgotten in the other would either go undescribed or be described twice.
    #[test]
    fn the_two_message_search_field_tables_agree() {
        let registry = crate::message_search::MessageSearchParameterRegistry::current();
        let projected = registry
            .parameters()
            .iter()
            .flat_map(|parameter| message_search_mcp_fields(parameter.parameter()))
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        let described = MESSAGE_SEARCH_MCP_FIELDS
            .iter()
            .map(|(field, _, _)| *field)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            described,
            projected,
            "described but not projected: {:?}; projected but not described: {:?}",
            described.difference(&projected).collect::<Vec<_>>(),
            projected.difference(&described).collect::<Vec<_>>()
        );

        // Each row names the parameter that field actually resolves to, so a role table cannot
        // quietly reassign a spelling to the wrong identity.
        for (field, parameter, _) in MESSAGE_SEARCH_MCP_FIELDS {
            assert!(
                message_search_mcp_fields(*parameter).contains(field),
                "{field} is listed under {parameter:?}, which does not project onto it"
            );
        }

        // One row per advertised field, and no duplicates: a field described twice would be
        // rendered twice, with the last write silently winning.
        assert_eq!(
            described.len(),
            MESSAGE_SEARCH_MCP_FIELDS.len(),
            "a field appears more than once in the role table"
        );
    }

    /// `index_refresh` is injected into every tool by the adapter rather than declared by any of
    /// them, which used to be marked on the wire as `x-aise-adapter-control`. No client reads
    /// that key, so the property it asserted is asserted here against the emitted catalogue.
    #[test]
    fn index_refresh_is_injected_into_every_advertised_tool() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let listed = handle_tools_list(Some(json!(1)), &config);
        let tools = listed["result"]["tools"].as_array().expect("tools list");
        assert_eq!(tools.len(), 8);
        for tool in tools {
            let name = tool["name"].as_str().expect("tool name");
            let control = &tool["inputSchema"]["properties"][ADAPTER_CONTROL_FIELD];
            assert_eq!(
                control["enum"],
                json!(["auto", "existing-only"]),
                "{name} does not advertise the adapter's index-read policy"
            );
            assert_eq!(control["default"], json!("auto"), "{name}");
        }
    }

    #[test]
    fn search_messages_requires_all_results_instead_of_zero_limit() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let bounded = search_messages_value(&json!({ "limit": 1 }), &config, &db);
        assert_eq!(bounded["page"]["returned"], 1);
        assert_eq!(bounded["page"]["next_offset"], 1);

        let error = tool_search_messages(&json!({ "limit": 0 }), &config, &db)
            .expect_err("limit=0 contradicts the advertised positive page-size contract");
        assert_eq!(
            error,
            "limit must be greater than zero; use all_results=true for every match"
        );

        let unbounded = search_messages_value(&json!({ "all_results": true }), &config, &db);
        assert!(unbounded["page"]["returned"]
            .as_u64()
            .is_some_and(|count| count > 1));
        assert!(unbounded["page"]["next_offset"].is_null());
        assert!(unbounded["page"]["limit"].is_null());

        let unbounded_from_second =
            search_messages_value(&json!({ "all_results": true, "offset": 1 }), &config, &db);
        assert_eq!(unbounded_from_second["page"]["offset"], 1);
        assert_eq!(
            unbounded_from_second["page"]["returned"].as_u64(),
            unbounded["page"]["returned"]
                .as_u64()
                .map(|count| count - 1),
            "all_results must preserve the caller's offset"
        );
    }
}
