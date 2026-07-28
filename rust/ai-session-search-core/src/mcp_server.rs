use std::borrow::Cow;
use std::future::Future;
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};

use crate::config::Config;
use crate::dates::{self, Bound};
use crate::db::Db;
use crate::inspect::InspectionOptions;
use crate::message_search::{
    ContextWindow, DetailLevel, FieldViewBudget, LineWindow, MatchViewBudget, MatchWindow,
    MessageContentExtent, MessageQuery, MessageSearchInclude, MessageSearchRequest,
    MessageSearchResponse, MessageTarget, PurposeSelection, ReceiptLevel, RequestedExtent,
    RequestedTimeRange, SequenceRange,
};
use crate::models::{MessageFilters, Provider, Role, SearchFilters, SessionRecord};
use crate::refs::{extract_refs_from_text, ref_summary};
use crate::service::SessionSearch;
use crate::service::{CatalogService, MessageService};
use crate::sql_query::{self, DbSchemaArgs, ResolvedDbQueryArgs};
use crate::util::{
    current_repo, normalize_path_prefix, render_posix_shell_command, resume_plan,
    select_message_lines, select_transcript_lines, truncate_for_display,
    truncate_for_display_with_extent,
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

/// Serve newline-delimited MCP JSON-RPC over standard input/output until EOF.
pub fn serve() -> anyhow::Result<()> {
    serve_server(McpServer::load()?)
}

/// Serve with configuration already resolved by an embedding CLI or API.
pub fn serve_with_config(config: Config) -> anyhow::Result<()> {
    serve_server(McpServer::new(config))
}

fn serve_server(mut server: McpServer) -> anyhow::Result<()> {
    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        server.handle_line(&line, |response| {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
            Ok::<(), io::Error>(())
        })?;
    }
    Ok(())
}

/// Stateful MCP request processor for embedding the server in alternate transports.
///
/// Callers retain one instance for the lifetime of a connection so the database is opened lazily
/// and reused across tool calls. [`handle_line`](Self::handle_line) never writes to stdout, making
/// it safe for Python bindings, tests, and future socket transports to own their I/O layer.
pub struct McpServer {
    config: Config,
    app: Option<SessionSearch>,
    client_supports_roots: bool,
    harness_roots: Vec<std::path::PathBuf>,
    pending_roots_request_id: Option<Value>,
    next_roots_request_id: u64,
    roots_error: Option<String>,
    advertised_tools: Option<Value>,
    refresh_worker: RefreshWorker,
    refresh_after_response: bool,
}

/// Official MCP SDK adapter around AI Session Search's transport-neutral state and tool semantics.
///
/// `rmcp` owns JSON-RPC framing, protocol negotiation, request concurrency, and cancellation.
/// The mutex protects the lazily opened SQLite application and root-authority state; individual
/// tool calls move blocking database work off the async runtime before this adapter becomes the
/// production stdio entry point.
pub struct OfficialMcpServer {
    inner: Arc<Mutex<McpServer>>,
}

impl OfficialMcpServer {
    pub fn new(config: Config) -> Self {
        Self {
            inner: Arc::new(Mutex::new(McpServer::new(config))),
        }
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
}

impl McpServer {
    /// Load configured provider and index settings without opening or refreshing the database.
    pub fn load() -> anyhow::Result<Self> {
        let config = Config::load()?;
        Ok(Self::new(config))
    }

    /// Create a server with explicit configuration for embedded and test use.
    pub fn new(config: Config) -> Self {
        Self {
            config,
            app: None,
            client_supports_roots: false,
            harness_roots: Vec::new(),
            pending_roots_request_id: None,
            next_roots_request_id: 1,
            roots_error: None,
            advertised_tools: None,
            refresh_worker: RefreshWorker::default(),
            refresh_after_response: false,
        }
    }

    /// Process and deliver one newline-delimited JSON-RPC frame.
    ///
    /// `deliver` receives a serialized response and must return only after the transport has
    /// flushed it. Blank lines and malformed JSON do not call `deliver`. Ordinary notifications
    /// return `false`; in restricted mode, initialization and roots-change notifications can
    /// deliver a server-originated `roots/list` request and return `true`. Automatic refresh starts
    /// only after successful delivery. Initialization is independent of transcript volume and
    /// index access.
    pub fn handle_line<E>(
        &mut self,
        line: &str,
        deliver: impl FnOnce(&str) -> Result<(), E>,
    ) -> anyhow::Result<bool>
    where
        E: std::fmt::Display,
    {
        let Some(response) = self.prepare_line(line)? else {
            return Ok(false);
        };
        if let Err(error) = deliver(&response) {
            self.refresh_after_response = false;
            anyhow::bail!("failed to deliver MCP response: {error}");
        }
        self.response_delivered();
        Ok(true)
    }

    fn prepare_line(&mut self, line: &str) -> anyhow::Result<Option<String>> {
        let line = line.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let request: Value = match serde_json::from_str(line) {
            Ok(request) => request,
            Err(_) => return Ok(None),
        };

        if self
            .pending_roots_request_id
            .as_ref()
            .is_some_and(|pending| request.get("id") == Some(pending))
            && request.get("method").is_none()
        {
            self.apply_roots_response(&request);
            return Ok(None);
        }

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let response = match method {
            "initialize" => {
                self.observe_client_capabilities(&params);
                handle_initialize(id.clone())
            }
            "tools/list" => {
                let response = handle_tools_list(id.clone(), &self.config);
                self.advertised_tools = Some(response["result"]["tools"].clone());
                response
            }
            "tools/call" => match validate_tool_call(&params, self.advertised_tools()) {
                Err(err) => tool_error_response(id.clone(), err),
                Ok(()) if is_schema_only_index_call(&params) => {
                    let args = params.get("arguments").cloned().unwrap_or(json!({}));
                    tool_call_result_response(
                        id.clone(),
                        tool_query_session_index(&args, &self.config),
                    )
                }
                Ok(()) if tool_requests_existing_only(&params) => {
                    let existing_only = (|| -> anyhow::Result<Value> {
                        if let Some(error) = &self.roots_error {
                            anyhow::bail!("invalid MCP roots authority: {error}");
                        }
                        let mut config = self.config.clone();
                        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
                        let mut app = None;
                        let app = open_mcp_app(&mut app, &config, &self.harness_roots)?;
                        Ok(handle_tools_call(
                            id.clone(),
                            &params,
                            app.config(),
                            app.database(),
                        ))
                    })();
                    match existing_only {
                        Ok(response) => response,
                        Err(err) => json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32603, "message": format!("failed to open existing session index: {err:#}") }
                        }),
                    }
                }
                Ok(()) => match self.open_app().and_then(|app| {
                    prepare_index_for_immediate_mcp_read(app)?;
                    Ok(app)
                }) {
                    Ok(app) => {
                        let response =
                            handle_tools_call(id.clone(), &params, app.config(), app.database());
                        self.refresh_after_response =
                            self.config.index.refresh == crate::config::IndexRefresh::Auto;
                        response
                    }
                    Err(err) => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": -32603, "message": format!("failed to prepare session index: {err:#}") }
                    }),
                },
            },
            // Cancellation is an optional MCP utility. This synchronous stdio implementation has
            // no in-flight request registry, so it may ignore cancellation for a request that
            // cannot be interrupted, as the specification permits. Closing stdin or terminating
            // the child remains the transport-level cancellation/cleanup path. Never respond to
            // either notification.
            "notifications/initialized" => return self.request_roots(),
            "notifications/roots/list_changed" => {
                if !self.client_supports_roots {
                    return Ok(None);
                }
                // Revoke the previous live roots before asking for replacements. A tool call that
                // arrives before the response can use only explicit configuration/current-dir
                // authority and therefore cannot retain access through a removed client root.
                self.app = None;
                self.harness_roots.clear();
                self.roots_error = None;
                return self.request_roots();
            }
            "notifications/cancelled" => return Ok(None),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unknown method: {method}") }
            }),
        };
        Ok(Some(serde_json::to_string(&response)?))
    }

    fn response_delivered(&mut self) {
        if std::mem::take(&mut self.refresh_after_response) {
            self.refresh_worker.schedule(self.config.clone());
        }
    }

    fn advertised_tools(&mut self) -> &Value {
        self.advertised_tools
            .get_or_insert_with(|| handle_tools_list(None, &self.config)["result"]["tools"].clone())
    }

    fn observe_client_capabilities(&mut self, params: &Value) {
        let roots = params
            .get("capabilities")
            .and_then(|value| value.get("roots"));
        self.client_supports_roots = roots.is_some_and(Value::is_object);
    }

    fn request_roots(&mut self) -> anyhow::Result<Option<String>> {
        if self.config.search.scope.mode != crate::config::SearchScopeMode::AllowedRoots
            || !self.client_supports_roots
            || self.pending_roots_request_id.is_some()
        {
            return Ok(None);
        }
        let id = Value::String(format!("aise-roots-{}", self.next_roots_request_id));
        self.next_roots_request_id = self.next_roots_request_id.saturating_add(1);
        self.pending_roots_request_id = Some(id.clone());
        Ok(Some(serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "roots/list",
            "params": {}
        }))?))
    }

    fn apply_roots_response(&mut self, response: &Value) {
        self.pending_roots_request_id = None;
        match parse_mcp_roots(response).and_then(|roots| {
            let inputs = mcp_access_inputs(&self.config, roots.clone())?;
            crate::search_scope::EffectiveAccessScope::resolve(
                &self.config.search.scope,
                inputs.clone(),
            )?;
            let replacement = self
                .app
                .is_some()
                .then(|| SessionSearch::open_with_access_inputs(self.config.clone(), inputs))
                .transpose()?;
            Ok((roots, replacement))
        }) {
            Ok((roots, replacement)) => {
                if let Some(app) = replacement {
                    self.app = Some(app);
                }
                self.harness_roots = roots;
                self.roots_error = None;
            }
            Err(error) => {
                self.app = None;
                self.harness_roots.clear();
                self.roots_error = Some(format!("{error:#}"));
            }
        }
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

fn tool_error_response(id: Option<Value>, error: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "isError": true,
            "content": [{ "type": "text", "text": error }]
        }
    })
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
    validate_schema_value(arguments, &tool["inputSchema"], tool_name, "arguments")
}

fn validate_schema_value(
    value: &Value,
    schema: &Value,
    tool_name: &str,
    path: &str,
) -> Result<(), String> {
    let invalid = |detail: String| format!("invalid {tool_name} {path}: {detail}");
    if let Some(branches) = schema.get("oneOf").and_then(Value::as_array) {
        let matching = branches
            .iter()
            .filter(|branch| validate_schema_value(value, branch, tool_name, path).is_ok())
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

fn parse_mcp_roots(response: &Value) -> anyhow::Result<Vec<std::path::PathBuf>> {
    if let Some(error) = response.get("error") {
        anyhow::bail!("roots/list failed: {error}");
    }
    let roots = response
        .get("result")
        .and_then(|result| result.get("roots"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("roots/list response must contain result.roots as an array")
        })?;
    roots
        .iter()
        .enumerate()
        .map(|(index, root)| {
            let uri = root
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("roots[{index}].uri must be a string"))?;
            let url = url::Url::parse(uri)
                .map_err(|error| anyhow::anyhow!("roots[{index}].uri is invalid: {error}"))?;
            if url.scheme() != "file" {
                anyhow::bail!("roots[{index}].uri must use the file scheme");
            }
            url.to_file_path()
                .map_err(|_| anyhow::anyhow!("roots[{index}].uri is not a local file path"))
        })
        .collect()
}

fn prepare_index_for_immediate_mcp_read(app: &SessionSearch) -> anyhow::Result<()> {
    let outcome = crate::indexer::prepare_index_for_read_now(app.config(), app.database());
    match outcome {
        Ok(None)
        | Ok(Some(crate::indexer::AutoReindexOutcome::Updated { .. }))
        | Ok(Some(crate::indexer::AutoReindexOutcome::SkippedFresh)) => Ok(()),
        Ok(Some(crate::indexer::AutoReindexOutcome::SkippedBusy)) => {
            eprintln!(
                "aise mcp serve: auto-reindex skipped because another process is writing; serving existing index"
            );
            Ok(())
        }
        Ok(Some(crate::indexer::AutoReindexOutcome::SkippedLockUnavailable { reason })) => {
            eprintln!(
                "aise mcp serve: auto-reindex skipped because the update lock is unavailable; serving existing index ({reason})"
            );
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[derive(Default)]
struct RefreshWorker {
    sender: Option<mpsc::SyncSender<()>>,
    cancel: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
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
            self.handle = Some(thread::spawn(move || {
                while receiver.recv().is_ok() {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    run_background_refresh(&config, &cancel);
                    while receiver.try_recv().is_ok() {}
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

fn handle_initialize(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {
                "tools": {}
            },
            "serverInfo": {
                "name": "ai-session-search",
                "title": "AI Session Search",
                // Single source of truth: the package version, never a hand-kept duplicate.
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": crate::integrations::agent_instructions()
        }
    })
}

fn provider_filter_schema(provider_values: &[&str], description: &str) -> Value {
    json!({
        "type": "string",
        "enum": provider_values,
        "description": description,
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
    json!({
        "type": "object",
        "oneOf": [
            {
                "properties": {
                    "session": session_record_meta_output_schema(),
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
                "properties": {
                    "session_id": { "type": "string" },
                    "anchor_seq": { "type": "integer" },
                    "cwd": { "type": ["string", "null"] },
                    "repo": { "type": ["string", "null"] },
                    "title": { "type": ["string", "null"] },
                    "session_metadata": session_record_meta_output_schema(),
                    "messages": { "type": "array", "items": focused_message_output_schema() }
                },
                "required": ["session_id", "anchor_seq", "cwd", "repo", "title", "session_metadata", "messages"],
                "additionalProperties": false
            },
            {
                "properties": {
                    "session": session_record_output_schema(),
                    "user_intent": { "type": "array", "items": message_preview_output_schema() },
                    "tool_activity": { "type": "array", "items": tool_activity_output_schema() },
                    "refs": { "type": "array", "items": ref_evidence_output_schema() },
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
            "resume_command": { "type": "string", "description": "Copy-pastable POSIX-shell command that resumes the session, byte-for-byte identical to the text content." },
            "cwd": { "type": ["string", "null"], "description": "Working directory the resume command changes into first, or null when none is recorded." }
        },
        "required": ["session_id", "resume_command", "cwd"],
        "additionalProperties": false
    })
}

fn focused_message_output_schema() -> Value {
    let mut properties = message_row_properties();
    properties.remove("session_id");
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

fn ref_evidence_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "seq": { "type": "integer", "minimum": 0 },
            "role": { "type": "string" },
            "tool_name": { "type": ["string", "null"] },
            "ref_summary": { "type": "string" },
            "refs": { "type": "array", "items": message_reference_output_schema() },
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
        "description": "Whether returned content is complete and which boundary was omitted. Original totals are null unless content is byte-for-byte complete, avoiding a second full-input scan for shortened rows.",
        "properties": {
            "complete": { "type": "boolean" },
            "omitted_start": { "type": "boolean" },
            "omitted_end": { "type": "boolean" },
            "returned_chars": { "type": "integer", "minimum": 0 },
            "returned_lines": { "type": "integer", "minimum": 0 },
            "original_chars": { "type": ["integer", "null"], "minimum": 0 },
            "original_lines": { "type": ["integer", "null"], "minimum": 0 }
        },
        "required": [
            "complete", "omitted_start", "omitted_end", "returned_chars", "returned_lines",
            "original_chars", "original_lines"
        ],
        "additionalProperties": false
    })
}

fn session_meta_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider_session_id": { "type": "string" },
            "cwd": { "type": "string" },
            "repo": { "type": "string" },
            "title": { "type": "string" },
            "updated_at": { "type": "string" },
            "last_message_at": { "type": "string" },
            "message_count": { "type": "integer", "minimum": 0 },
            "parse_warning": { "type": "string" }
        },
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
    let correction_match = json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string" },
            "provider": { "type": "string" },
            "ts": { "type": ["string", "null"] },
            "policy_name": { "type": "string" },
            "category": { "type": "string" },
            "matched_text": { "type": "string" },
            "content": { "type": "string" }
        },
        "required": ["session_id", "provider", "ts", "policy_name", "category", "matched_text", "content"],
        "additionalProperties": false
    });
    json!({
        "type": "object",
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
                            "selected_location": selected_location,
                            "execution_source": execution_source
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
                                    "receipt": receipt.clone(),
                                    "report": {
                                        "type": "object",
                                        "properties": {
                                            "policies": { "type": "array", "items": receipt },
                                            "matches": { "type": "array", "items": correction_match }
                                        },
                                        "required": ["policies", "matches"],
                                        "additionalProperties": false
                                    }
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
    let field_extent = json!({
        "type": "object",
        "properties": {
            "additional_field_text": {
                "type": "string",
                "enum": ["none", "before", "after", "before_and_after"]
            },
            "field_total_chars": { "type": ["integer", "null"], "minimum": 0 },
            "coordinate_unit": { "const": "unicode_scalar" }
        },
        "required": ["additional_field_text", "field_total_chars", "coordinate_unit"],
        "additionalProperties": false
    });
    let field_view = json!({
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
            "extent": field_extent
        },
        "required": ["text", "field_start_char", "field_end_char_exclusive", "extent"],
        "additionalProperties": false
    });
    let message_ref = json!({
        "type": "object",
        "properties": {
            "session_id": { "type": "string" },
            "message_seq": { "type": "integer" }
        },
        "required": ["session_id", "message_seq"],
        "additionalProperties": false
    });
    let message_metadata = json!({
        "type": "object",
        "properties": {
            "provider": { "type": "string", "enum": crate::source::PROVIDERS },
            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"] },
            "kind": { "type": "string", "enum": message_kind_values() }
        },
        "required": ["provider", "role", "kind"],
        "additionalProperties": false
    });
    let context_message = json!({
        "type": "object",
        "properties": {
            "message_ref": message_ref.clone(),
            "message_metadata": message_metadata.clone(),
            "timestamp": { "type": ["string", "null"], "format": "date-time" },
            "tool_name": { "type": ["string", "null"] },
            "tool_call_id": { "type": ["string", "null"] },
            "presentation": {
                "type": "object",
                "properties": {
                    "field_view": field_view.clone()
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
                            "coordinate_unit": { "const": "unicode_scalar" }
                        },
                        "required": [
                            "text", "field_start_char", "field_end_char_exclusive", "coordinate_unit"
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
                    "parsed_references": { "type": "array", "items": ref_evidence_output_schema() }
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
        "properties": {
            "response_schema_version": { "type": "integer", "description": "Version of this search_messages response contract, independent of the SQLite database schema version." },
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
        "required": ["response_schema_version", "effective_request", "results", "page"],
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
            "index_update": {
                "type": ["object", "null"],
                "description": "Actionable automatic index-update status. null means no action is needed; normal completed, fresh, busy, and cancelled maintenance stays silent.",
                "properties": {
                    "state": { "type": "string", "enum": ["in_progress", "attention_required"], "description": "in_progress means searches remain available on the compatible existing index; attention_required means automatic maintenance failed or its status cannot be read." },
                    "started_at": { "type": "string", "format": "date-time" },
                    "message": { "type": "string", "description": "Concrete status or failure context." },
                    "next_command": { "type": ["string", "null"], "description": "Exact recovery command when one is safe and applicable; otherwise null." }
                },
                "required": ["state", "started_at", "message", "next_command"],
                "additionalProperties": false
            },
            "providers": { "type": "array", "description": "Discovery, parser, index, and resume status for every supported provider.", "items": provider_health_output_schema() }
        },
        "required": ["db_path", "parser_health", "repairable_stale_sessions", "unavailable_stale_sessions", "unindexed_files", "repair_commands", "index_update", "providers"],
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
    let provider_filter_description = format!(
        "Filter to one session source: {provider_summary}. Omit provider to include all eight sources."
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
        "Expert read-only SQL over the SQLite index for {provider_summary}. Prefer search_messages for content or regex search because it uses the FTS/trigram planner and returns context. Bounded live schema summary: {schema_summary}. Omit sql to list schema objects; use schema_table for one table's columns; pass sql only for one row-returning SELECT/WITH statement."
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
                                "description": "Lower time bound: sessions last updated at or after this. Calendar/relative periods use UTC; an exact RFC 3339 timestamp honors Z or its explicit offset and preserves fractional seconds. Examples: '2026-01-15', '2026-01' (whole month), '202X' (whole decade), '7d' (last 7 days), 'yesterday', '2026-01-15T14:30:25.123Z'. Default: no lower bound."
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper time bound, inclusive: sessions last updated at or before this. Same precision and timezone rules as since. Default: no upper bound."
                            },
                            "when": {
                                "type": "string",
                                "description": "Single UTC period used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. An exact RFC 3339 value selects that instant at its stated precision. Do not combine with since/until."
                            },
                            "limit": {
                                "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(),
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Accepts a positive count or 0.", config.mcp.search_sessions_limit),
                                "default": config.mcp.search_sessions_limit
                            },
                            "offset": {
                                "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(),
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
                                "description": "Message sequence number copied from a search_messages hit. This is the same value a search_messages hit exposes as its `seq` field (the input name message_seq and the hit field seq refer to one identifier; a future release may unify the spelling). Returns a focused message-context result instead of transcript lines."
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
                            "preview_chars": { "type": "integer", "minimum": 1, "maximum": max_mcp_numeric_usize(), "description": format!("Maximum characters per concise message/tool/ref preview in summary output and focused message context (default {}). Not used for transcript output.", config.mcp.preview_chars.max(1)), "default": config.mcp.preview_chars.max(1) },
                            "lines_per_message": {
                                "type": "integer",
                                "description": format!("With message_seq: limit each returned message's displayed content (positive keeps its first N lines, negative keeps its last N lines, 0 keeps complete content; default {}). This presentation window does not change context membership or reference extraction. Use it to keep long tool output around one turn skimmable. It bounds each returned message on its own; use transcript_lines to window a whole session transcript.", config.mcp.lines_per_message),
                                "default": config.mcp.lines_per_message
                            },
                            "response_format": {
                                "type": "string",
                                "enum": ["concise", "detailed"],
                                "description": "When message_seq is provided, 'concise' (default) trims each message to a snippet; 'detailed' returns full text.",
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
                                "description": "Lower time bound: sessions last updated at or after this. Calendar/relative periods use UTC; an exact RFC 3339 timestamp honors Z or its explicit offset and preserves fractional seconds. Examples: '2026-01-15', '202X' (whole decade), '7d' (last 7 days), 'yesterday', '2026-01-15T14:30:25.123Z'. Default: no lower bound."
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper time bound, inclusive: sessions last updated at or before this. Same precision and timezone rules as since. Default: no upper bound."
                            },
                            "when": {
                                "type": "string",
                                "description": "Single UTC period used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. An exact RFC 3339 value selects that instant at its stated precision. Do not combine with since/until."
                            },
                            "limit": {
                                "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(),
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Accepts a positive count or 0.", config.mcp.list_sessions_limit),
                                "default": config.mcp.list_sessions_limit
                            },
                            "offset": {
                                "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(),
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
                    "description": "Find exact message evidence across local AI session history. Search content, tool_name, or one tool_argument path. structuredContent is authoritative: effective_request states the resolved interpretation and budgets, results carry message_ref plus field/match views and exact literal coordinates, and page.next_offset is the next offset argument when more results exist. MCP applies a finite configured page when limit is omitted; pass a positive limit or explicit non-fuzzy all_results. context adds neighboring turns without changing result membership. Use get_session(session_id, message_seq) for the complete focused message.",
                    "outputSchema": search_messages_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Text or pattern to find. Omit only with query_mode='literal' to list messages selected by the other predicates." },
                            "query_mode": { "type": "string", "enum": ["literal", "regex", "fuzzy"], "description": "Interpret query as a case-insensitive literal substring, Rust regex, or bounded fuzzy pattern. Defaults to literal.", "default": "literal" },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"], "description": "Only this message role: user (non-command prompts), assistant, tool (tool calls/results), slash (human-entered commands such as /goal), or compaction. Omit for all roles." },
                            "kind": { "type": "string", "enum": message_kind_values(), "description": "Only this semantic message kind: conversation (ordinary user/assistant turns), compaction (auto-generated summary messages), tool_call (a tool invocation, matched without its result), tool_result (the output a tool returned), harness_notice (Stop-hook feedback, PreToolUse blocks, local-command caveats, task notifications: what the harness told the agent, not what the user wrote), or unknown (a message whose kind could not be classified). Omit for all kinds except harness_notice. Alias for a one-element kinds array; pass kinds to select several." },
                            "field": { "type": "string", "enum": ["content", "tool_name", "tool_argument"], "description": "Select the field searched by query: content (default), the canonical tool_name, or tool_argument for one canonical tool argument selected by argument_path.", "default": "content" },
                            "argument_path": { "type": "string", "description": "RFC 6901 JSON pointer relative to canonical tool-call args, e.g. '/cmd' or '/request/path'. Required only when field='tool_argument'." },
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "tool_name_contains": { "type": "string", "description": "Additionally require canonical tool_name to contain this text, independent of the searched field." },
                            "session_id": { "type": "string", "description": "Exact session ID or unique prefix. Use this when chaining from search_messages/get_session results." },
                            "workspace_path_prefix": { "type": "string", "description": "Only messages whose session working directory or repository root starts with this path." },
                            "transcript_path_prefix": { "type": "string", "description": "Only messages whose transcript storage path starts with this path." },
                            "exclude_workspace_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude session working-directory or repository-root prefixes before matching and paging." },
                            "exclude_transcript_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude transcript storage prefixes before matching and paging." },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" }, "description": "Exclude exact session IDs. Applied before limit/context. Omit for no session exclusions." },
                            "seq_from": { "type": "integer", "minimum": 0, "description": "Lower inclusive message sequence bound. Requires session_id because seq values are session-local. Pair with seq_to to read one session in non-overlapping chunks (e.g. 0..499, then 500..999) without re-reading turns." },
                            "seq_to": { "type": "integer", "minimum": 0, "description": "Upper inclusive message sequence bound. Requires session_id because seq values are session-local. See seq_from for non-overlapping chunked reads." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. Calendar/relative periods use UTC; an exact RFC 3339 timestamp honors Z or its explicit offset and preserves fractional seconds. Examples: '2026-01-15', '202X', '7d', 'yesterday', '2026-01-15T14:30:25.123Z'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound, inclusive: messages at or before this. Same precision and timezone rules as since. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single UTC period used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. An exact RFC 3339 value selects that instant at its stated precision. Do not combine with since/until." },
                            "include_compaction": { "type": "boolean", "description": "Include auto-generated summary messages. Defaults to true.", "default": true },
                            "kinds": { "type": "array", "items": { "type": "string", "enum": message_kind_values() }, "description": "Which semantic message classes to return: conversation (ordinary user/assistant turns), compaction (auto-generated summary messages), tool_call (a tool invocation, matched without its result), tool_result (the output a tool returned), harness_notice (Stop-hook feedback, PreToolUse blocks, local-command caveats, task notifications: what the harness told the agent, not what the user wrote), and unknown (a message whose kind could not be classified). Omit to get every class except harness_notice. Name classes to change that: [\"harness_notice\"] answers why an agent stopped, looped, or was blocked; [\"conversation\", \"harness_notice\"] returns both. An empty array selects nothing and is rejected rather than silently returning no matches. This is the single class filter; kind is its one-value alias." },
                            "match_window": { "type": "string", "enum": ["earliest", "latest"], "description": "Select earliest matches, or the latest matches within one session and present them chronologically." },
                            "context": { "type": "integer", "minimum": 0, "description": "Return this many turns before and after each match in the same call (default 0). Use this for immediate one-step context.", "default": 0 },
                            "context_before": { "type": "integer", "minimum": 0, "description": "Override the number of preceding messages." },
                            "context_after": { "type": "integer", "minimum": 0, "description": "Override the number of following messages." },
                            "lines_per_message": { "type": "integer", "description": format!("Limit each result's selected-field view by lines: positive keeps the first N, negative keeps the last N, and 0 applies no line limit (configured MCP default {}). It applies before field_view's character budget and never changes matching, ordering, result count, context membership, or includes. Conflicts with detail.", config.mcp.lines_per_message), "default": config.mcp.lines_per_message },
                            "detail": { "type": "string", "enum": ["compact", "full"], "description": "Presentation preset only. compact uses MCP's bounded field and match views; full removes the field character cap. It never changes result count, context, includes, or receipts. Conflicts with lines_per_message, field_view, and match_view." },
                            "field_view": {
                                "description": format!("Selected-field boundary view budget after the line window. no_char_limit removes only the character cap; max_chars retains at most that many Unicode scalar characters. The configured MCP default is max_chars={}. This never changes matching or page membership.", config.mcp.preview_chars.max(1)),
                                "default": {
                                    "kind": "max_chars",
                                    "max_chars": config.mcp.preview_chars.max(1)
                                },
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": { "kind": { "const": "no_char_limit" } },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "const": "max_chars" },
                                            "max_chars": { "type": "integer", "minimum": 1, "maximum": max_mcp_numeric_usize() }
                                        },
                                        "required": ["kind", "max_chars"],
                                        "additionalProperties": false
                                    }
                                ]
                            },
                            "match_view": {
                                "description": "Independent match-centered view budget. minimal_span keeps the complete literal/regex occurrence or smallest fuzzy marker span; max_chars adds surrounding selected-field text without changing matching.",
                                "oneOf": [
                                    {
                                        "type": "object",
                                        "properties": { "kind": { "const": "minimal_span" } },
                                        "required": ["kind"],
                                        "additionalProperties": false
                                    },
                                    {
                                        "type": "object",
                                        "properties": {
                                            "kind": { "const": "max_chars" },
                                            "max_chars": { "type": "integer", "minimum": 1, "maximum": max_mcp_numeric_usize() }
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
                            "purpose_version": { "type": "integer", "minimum": 1, "description": "Required configured purpose version; requires purpose." },
                            "receipt_level": { "type": "string", "enum": ["none", "summary", "full"], "description": "none omits diagnostics; summary includes planner diagnostics; full adds resolved parameter origins." },
                            "limit": { "type": "integer", "minimum": 1, "maximum": max_mcp_numeric_usize(), "description": format!("Positive page size. Omit to use the configured MCP default of {}.", config.mcp.search_messages_limit.max(1)), "default": config.mcp.search_messages_limit.max(1) },
                            "all_results": { "type": "boolean", "description": "Return every literal, regex, or no-text match. Defaults to false; conflicts with limit and is invalid for fuzzy search.", "default": false },
                            "offset": { "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(), "description": "Skip this many matches before returning, to page through results (default 0). Accepts a positive count or 0.", "default": 0 }
                        },
                        "additionalProperties": false
                    }
                },
                {
                    "name": "run_skill_capability",
                    "annotations": read_only_tool_annotations(),
                    "description": format!("Execute the deterministic message-classification capability declared by one Aise skill package across {provider_summary}. Aise reads capability.toml only; the MCP client or AI harness loads and follows SKILL.md. Select corrections or another catalog package by name, or pass a package path authorized by [skills].search_paths. Selected capability.toml documents share a 1 MiB aggregate parsing safety ceiling; exceeding it returns byte counts and guidance rather than truncating rules or results. Defaults to user messages in user-started sessions. Returns the resolved package, capability and policy receipts, exact digests, matches, and pagination. For corrections, this is equivalent to `aise skills corrections --format json`."),
                    "outputSchema": run_skill_capability_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "skill": skill_selector_input_schema(),
                            "additional_skills": { "type": "array", "uniqueItems": true, "items": skill_selector_input_schema(), "description": "Additional skill packages whose rules are evaluated after the primary package. Every package must declare the same capability type. This does not load or follow their SKILL.md instructions." },
                            "session_kinds": { "type": "array", "items": { "type": "string", "enum": session_kind_values() }, "description": "Which session classes to scan. Omit for user-started sessions only: in a spawned subagent run, 'user' rows contain the calling agent's delegation prompt rather than text a person entered. Pass [\"user\", \"subagent\"] to scan both. This default differs from search_messages and list_sessions, which return both classes." },
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "session_id": { "type": "string", "description": "Exact session ID or unique prefix. Use to scope the capability run to one session found by search_sessions." },
                            "workspace_path_prefix": { "type": "string", "description": "Only sessions whose working directory or repository root starts with this path. Use to scope the capability run to one project." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. Calendar/relative periods use UTC. Examples: '2026-01-15', '7d', 'yesterday'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound, inclusive: messages at or before this. Same rules as since. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single UTC period used as both bounds, e.g. '2026-01', '7d', 'yesterday'. Do not combine with since/until." },
                            "limit": { "type": "integer", "minimum": 1, "maximum": max_mcp_numeric_usize(), "description": format!("Positive page size. Omit to use the configured MCP default of {}. Use all_results for every match.", config.mcp.run_message_classification_limit), "default": config.mcp.run_message_classification_limit },
                            "all_results": { "type": "boolean", "description": "Return every match rather than one page. Defaults to false; conflicts with limit. Each match carries a whole user message, so prefer paging when the range is wide.", "default": false },
                            "offset": { "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(), "description": "Skip this many matches before returning, newest first, to page through results (default 0). Accepts a positive count or 0.", "default": 0 }
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
                            "limit": { "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(), "description": format!("Maximum rows to return after the SQL statement runs (default {}). 0 means unlimited; prefer adding LIMIT in SQL for expensive queries. Accepts a positive count or 0.", config.db.query_limit), "default": config.db.query_limit },
                            "offset": { "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(), "description": "Skip this many rows after the SQL statement runs (default 0). Prefer SQL LIMIT/OFFSET for expensive queries. Accepts a positive count or 0.", "default": 0 },
                            "timeout_ms": { "type": "integer", "minimum": 0, "description": format!("MCP-only raw-SQL availability guard in milliseconds (default {}). 0 disables interruption. This is independent of native CLI/Rust SQL defaults and does not apply to indexed search tools.", config.mcp.query_timeout_ms), "default": config.mcp.query_timeout_ms },
                            "max_cell_chars": { "type": "integer", "minimum": 0, "maximum": max_mcp_numeric_usize(), "description": format!("Maximum characters per string cell in the JSON response. 0 disables cell truncation. Default {}.", config.mcp.query_max_cell_chars), "default": config.mcp.query_max_cell_chars }
                        },
                        "additionalProperties": false
                    }
                }
            ]
        }
    });
    add_index_refresh_controls(&mut response);
    response
}

fn handle_tools_call(id: Option<Value>, params: &Value, config: &Config, db: &Db) -> Value {
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "search_sessions" => tool_search_sessions(&args, config, db),
        "get_session" => tool_get_session(&args, config, db),
        "list_sessions" => tool_list_sessions(&args, config, db),
        "get_resume_command" => tool_get_resume_command(&args, db),
        "search_messages" => tool_search_messages(&args, config, db),
        "run_skill_capability" => tool_run_skill_capability(&args, config, db),
        "get_index_status" => crate::diagnostics::collect(config, db)
            .map_err(|error| format!("{error:#}"))
            .and_then(|status| serde_json::to_value(status).map_err(|error| format!("{error:#}")))
            .and_then(ToolResponse::structured),
        "query_session_index" => tool_query_session_index(&args, config),
        // Derive the served names from the advertised list rather than restating them, so this
        // recovery hint can never drift from what tools/list actually publishes.
        _ => Err(unknown_tool_message(
            tool_name,
            &handle_tools_list(None, config)["result"]["tools"],
        )),
    };

    tool_call_result_response(id, result)
}

fn tool_call_result_response(id: Option<Value>, result: Result<ToolResponse, String>) -> Value {
    match result {
        Ok(content) => {
            let mut result = json!({
                "content": [{ "type": "text", "text": content.text }]
            });
            if let Some(structured) = content.structured_content {
                result["structuredContent"] = structured;
            }
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result,
            })
        }
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "isError": true,
                "content": [{ "type": "text", "text": err }]
            }
        }),
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
        provider: parse_opt_enum::<Provider>(args, "provider")?,
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
    let mut run = crate::service::AnalysisService::new(config, db)
        .run_skill(&crate::skill_run::SkillRunQuery {
            skill: primary,
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
    let next_offset = if has_more {
        Some(offset.checked_add(returned).ok_or_else(|| {
            "run_skill_capability offset overflowed while computing the next page".to_string()
        })?)
    } else {
        None
    };
    let value = json!({
        "run": run,
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

fn tool_query_session_index(args: &Value, config: &Config) -> Result<ToolResponse, String> {
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
        let result = sql_query::schema_path(
            &config.db_path(),
            config.index.busy_timeout_ms,
            &schema_args,
        )
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
    let result =
        sql_query::query_path(&config.db_path(), config.index.busy_timeout_ms, &query_args)
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
            "description": "No message-search purposes are configured in this server. Omit purpose, or configure [search.purposes.<name>] before selecting one."
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
            "index_refresh".to_string(),
            json!({
                "type": "string",
                "enum": ["auto", "existing-only"],
                "default": "auto",
                "description": "Index-read policy for this call. auto uses normal configured preparation and may discover/index new transcript data; existing-only opens the compatible SQLite index read-only, performs no discovery, indexing, migration, or background refresh, and leaves the server's reusable auto-refresh app unopened."
            }),
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
        "maximum": max_mcp_numeric_usize(),
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

fn parse_budget_max_chars(
    object: &serde_json::Map<String, Value>,
    parameter: &str,
) -> Result<usize, String> {
    let value = object
        .get("max_chars")
        .ok_or_else(|| format!("{parameter}.max_chars is required when kind=max_chars"))?;
    let value = value
        .as_u64()
        .ok_or_else(|| format!("{parameter}.max_chars must be a positive integer"))?;
    let maximum =
        u64::try_from(max_mcp_numeric_usize()).expect("MCP numeric maximum must fit in u64");
    if value == 0 || value > maximum {
        return Err(format!(
            "{parameter}.max_chars must be an integer from 1 through {maximum}; got {value}"
        ));
    }
    usize::try_from(value).map_err(|_| {
        format!("{parameter}.max_chars exceeds this platform's supported integer range")
    })
}

fn reject_budget_unknown_fields(
    object: &serde_json::Map<String, Value>,
    parameter: &str,
    accepts_max_chars: bool,
) -> Result<(), String> {
    for key in object.keys() {
        if key != "kind" && !(accepts_max_chars && key == "max_chars") {
            return Err(format!(
                "{parameter} contains unknown field {key:?}; accepted fields are {}",
                if accepts_max_chars {
                    "kind and max_chars"
                } else {
                    "kind"
                }
            ));
        }
    }
    Ok(())
}

fn parse_field_view_budget(args: &Value) -> Result<Option<FieldViewBudget>, String> {
    let Some(value) = args.get("field_view") else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| "field_view must be an object with a kind field".to_string())?;
    match object.get("kind").and_then(Value::as_str) {
        Some("no_char_limit") => {
            reject_budget_unknown_fields(object, "field_view", false)?;
            Ok(Some(FieldViewBudget::NoCharLimit))
        }
        Some("max_chars") => {
            reject_budget_unknown_fields(object, "field_view", true)?;
            FieldViewBudget::max_chars(parse_budget_max_chars(object, "field_view")?)
                .map(Some)
                .map_err(|error| error.to_string())
        }
        Some(other) => Err(format!(
            "field_view.kind must be no_char_limit or max_chars; got {other:?}"
        )),
        None => Err("field_view.kind is required".to_string()),
    }
}

fn parse_match_view_budget(args: &Value) -> Result<Option<MatchViewBudget>, String> {
    let Some(value) = args.get("match_view") else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| "match_view must be an object with a kind field".to_string())?;
    match object.get("kind").and_then(Value::as_str) {
        Some("minimal_span") => {
            reject_budget_unknown_fields(object, "match_view", false)?;
            Ok(Some(MatchViewBudget::MinimalSpan))
        }
        Some("max_chars") => {
            reject_budget_unknown_fields(object, "match_view", true)?;
            MatchViewBudget::max_chars(parse_budget_max_chars(object, "match_view")?)
                .map(Some)
                .map_err(|error| error.to_string())
        }
        Some(other) => Err(format!(
            "match_view.kind must be minimal_span or max_chars; got {other:?}"
        )),
        None => Err("match_view.kind is required".to_string()),
    }
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

fn tool_search_messages(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
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
    if let Some(provider) = parse_opt_enum::<Provider>(args, "provider")? {
        builder = builder.provider(provider);
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
        .search(builder.build().map_err(|error| error.to_string())?)
        .map_err(|error| format!("{error:#}"))?;
    let text = message_search_text_summary(&response);
    let structured =
        serde_json::to_value(response.document()).map_err(|error| format!("{error:#}"))?;
    Ok(ToolResponse::structured_with_text(text, structured))
}

fn message_search_text_summary(response: &MessageSearchResponse) -> String {
    use std::fmt::Write as _;

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
        let _ = writeln!(
            summary,
            "Text shows the first {} of {returned} returned results; structuredContent contains all {returned}.",
            MESSAGE_SEARCH_TEXT_RESULT_LIMIT,
        );
    }
    summary
}

/// How message content is shaped for one response: full or concise preview, optional refs,
/// and the per-message line cap. Parsed once per tool call from the shared argument names.
struct MessagePresentation {
    detailed: bool,
    include_refs: bool,
    preview_chars: usize,
    lines_per_message: i64,
}

struct PresentedMessageContent {
    content: String,
    extent: MessageContentExtent,
}

impl MessagePresentation {
    fn from_args(args: &Value, config: &Config) -> Result<Self, String> {
        Ok(Self {
            detailed: args.get("response_format").and_then(Value::as_str) == Some("detailed"),
            include_refs: mcp_bool_arg(args, "include_refs", false),
            preview_chars: mcp_positive_usize_arg(
                args,
                "preview_chars",
                config.mcp.preview_chars.max(1),
            )?,
            lines_per_message: mcp_i64_arg(args, "lines_per_message", config.mcp.lines_per_message),
        })
    }

    /// Per-message line cap first (head/tail selection), then concise char preview if requested.
    /// Refs are always extracted from full content so a cap never hides references.
    fn trim_with_extent(&self, content: &str) -> PresentedMessageContent {
        let capped = if self.lines_per_message == 0 {
            Cow::Borrowed(content)
        } else {
            Cow::Owned(select_message_lines(content, self.lines_per_message))
        };
        if self.detailed {
            let extent = MessageContentExtent::describe(
                content,
                capped.as_ref(),
                capped.as_ref(),
                self.lines_per_message,
                false,
            );
            return PresentedMessageContent {
                content: capped.into_owned(),
                extent,
            };
        }
        let (returned, character_truncated) =
            truncate_for_display_with_extent(capped.as_ref(), self.preview_chars);
        let extent = MessageContentExtent::describe(
            content,
            capped.as_ref(),
            &returned,
            self.lines_per_message,
            character_truncated,
        );
        PresentedMessageContent {
            content: returned,
            extent,
        }
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
            let presented = presentation.trim_with_extent(&c.content);
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
            row
        })
        .collect();
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
            let presented = presentation.trim_with_extent(&c.content);
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
            row
        })
        .collect();
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

    fn search_messages_value(args: &Value, config: &Config, db: &Db) -> Value {
        structured(tool_search_messages(args, config, db).unwrap())
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

    fn deliver_line(server: &mut McpServer, line: &str) -> Option<String> {
        let mut response = None;
        server
            .handle_line(line, |serialized| {
                response = Some(serialized.to_string());
                Ok::<(), anyhow::Error>(())
            })
            .unwrap();
        response
    }

    fn call_tool(name: &str, arguments: Value, config: &Config, db: &Db) -> Value {
        handle_tools_call(
            Some(json!(1)),
            &json!({ "name": name, "arguments": arguments }),
            config,
            db,
        )
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
            context_view["extent"]["additional_field_text"], "after",
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
    fn existing_only_tool_call_uses_a_temporary_read_only_app_and_never_schedules_refresh() {
        let (dir, db) = fixture();
        drop(db);
        let config = config_for_fixture(&dir);
        let mut server = McpServer::new(config);

        let response = deliver_line(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "search_sessions",
                    "arguments": {
                        "query": "Proj",
                        "index_refresh": "existing-only"
                    }
                }
            })
            .to_string(),
        )
        .expect("request response");
        let response: Value = serde_json::from_str(&response).unwrap();

        assert_eq!(response["result"]["structuredContent"]["returned"], 1);
        assert!(
            server.app.is_none(),
            "per-call existing-only must not reuse or create the writable auto-refresh app"
        );
        assert!(!server.refresh_after_response);
        assert!(server.refresh_worker.handle.is_none());
        let tool = tool_input_schema(&server.config, "search_sessions");
        assert_eq!(
            tool["inputSchema"]["properties"]["index_refresh"]["enum"],
            json!(["auto", "existing-only"])
        );
    }

    #[test]
    fn search_messages_runtime_fields_are_declared_by_the_output_schema() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let schema = search_messages_output_schema();

        for args in [
            json!({ "query": "hello" }),
            json!({ "query": "helo", "query_mode": "fuzzy" }),
            json!({
                "query": "alpha",
                "context": 1,
                "include": [
                    "normalized_session_metadata",
                    "parsed_references",
                    "runtime_diagnostics"
                ],
                "receipt_level": "full"
            }),
        ] {
            let output = search_messages_value(&args, &config, &db);
            validate_schema_value(&output, &schema, "search_messages", "structuredContent")
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
        assert_eq!(field_view["extent"]["additional_field_text"], "after");
        assert!(field_view["text"].as_str().unwrap().chars().count() <= 80);
        assert!(
            field_view["extent"]["field_total_chars"]
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
        let order_doc = order["description"].as_str().unwrap();
        assert!(
            order_doc.contains("within one session") && order_doc.contains("chronologically"),
            "match-window doc states latest scope and presentation: {order_doc}"
        );

        // Page continuation uses the response's explicit next offset, while focused message
        // expansion uses the separate session/message identity.
        let tool_doc = tool["description"].as_str().unwrap();
        assert!(
            tool_doc.contains("page.next_offset") && tool_doc.contains("next offset argument"),
            "search_messages description advertises offset continuation: {tool_doc}"
        );

        // The kind filter documents every one of its enum values, not just one, so a caller
        // can choose conversation/compaction/tool_call/tool_result/unknown without guessing.
        let kind = &tool["inputSchema"]["properties"]["kind"];
        let kind_doc = kind["description"].as_str().unwrap();
        for value in [
            "conversation",
            "compaction",
            "tool_call",
            "tool_result",
            "unknown",
        ] {
            assert!(
                kind_doc.contains(value),
                "kind description defines the {value:?} value: {kind_doc}"
            );
        }
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
            ("role", check::<crate::models::Role>),
            ("field", check::<crate::models::SearchField>),
            ("index_refresh", check::<crate::config::IndexRefresh>),
        ];
        // Advertised vocabularies with no Rust enum behind them: each is matched inline by the
        // handler that reads it, so there is no second list to drift from.
        let presentation_only = [
            "detail",
            "include",
            "index_update",
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

        // task 4: cross-reference message_seq (input) with the hit's seq field without renaming.
        let message_seq_doc = properties["message_seq"]["description"].as_str().unwrap();
        assert!(
            message_seq_doc.contains("seq"),
            "message_seq doc cross-references the hit seq field: {message_seq_doc}"
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
    fn initialize_advertises_protocol_and_tools_capability() {
        let v = handle_initialize(Some(json!(1)));
        let r = &v["result"];
        assert_eq!(r["protocolVersion"], "2024-11-05");
        assert_eq!(r["serverInfo"]["name"], "ai-session-search");
        assert_eq!(r["serverInfo"]["title"], "AI Session Search");
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["instructions"], crate::integrations::agent_instructions());
        assert!(r["instructions"].as_str().unwrap().chars().count() <= 512);
    }

    #[test]
    fn production_mcp_adapter_implements_the_official_rmcp_server_contract() {
        fn assert_official_server<T: rmcp::ServerHandler>() {}

        assert_official_server::<OfficialMcpServer>();
    }

    #[test]
    fn official_rmcp_transport_negotiates_and_serves_the_canonical_tool_catalogue() {
        use rmcp::ServiceExt as _;

        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let config = Config::default();
            let (server_transport, client_transport) = tokio::io::duplex(64 * 1024);
            let server = OfficialMcpServer::new(config);
            let server_task = tokio::spawn(async move {
                server
                    .serve(server_transport)
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

            client.cancel().await.expect("client shutdown");
            server_task.await.unwrap().expect("server shutdown");
        });
    }

    #[test]
    fn restricted_mcp_scope_tracks_roots_list_and_revokes_before_list_changes() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("allowed");
        let hidden = dir.path().join("hidden");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&hidden).unwrap();
        let db_path = dir.path().join("index.db");
        let db = Db::open(&db_path).unwrap();
        for (id, workspace) in [("claude:allowed", &allowed), ("claude:hidden", &hidden)] {
            let mut parsed = minimal_record(
                Provider::Claude,
                &dir.path().join(format!("{id}.jsonl")),
                String::new(),
            );
            parsed.session.id = id.into();
            parsed.session.provider_session_id = id.replace(':', "-");
            parsed.session.cwd = Some(workspace.to_string_lossy().into_owned());
            parsed.session.repo_root = Some(workspace.to_string_lossy().into_owned());
            db.upsert_session(&parsed, 0, 0).unwrap();
        }
        drop(db);

        let mut config = Config::default();
        config.index.db_path = Some(db_path.to_string_lossy().into_owned());
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        config.search.scope.mode = crate::config::SearchScopeMode::AllowedRoots;
        config.search.scope.roots.clear();
        config.search.scope.include_invocation_directory = false;
        let mut server = McpServer::new(config);

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "roots": { "listChanged": true } },
                "clientInfo": { "name": "scope-test", "version": "1" }
            }
        });
        assert!(deliver_line(&mut server, &initialize.to_string()).is_some());
        let roots_request = parse(
            &deliver_line(
                &mut server,
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string(),
            )
            .expect("server requests roots after initialization"),
        );
        assert_eq!(roots_request["method"], "roots/list");

        let before_roots = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": { "name": "list_sessions", "arguments": {} }
                })
                .to_string(),
            )
            .unwrap(),
        );
        assert!(before_roots["error"]["message"]
            .as_str()
            .unwrap()
            .contains("resolved no authoritative roots"));

        let allowed_uri = url::Url::from_directory_path(&allowed).unwrap().to_string();
        assert!(deliver_line(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": roots_request["id"],
                "result": { "roots": [{ "uri": allowed_uri, "name": "Allowed" }] }
            })
            .to_string(),
        )
        .is_none());
        let list = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": { "name": "list_sessions", "arguments": {} }
                })
                .to_string(),
            )
            .unwrap(),
        );
        assert_eq!(
            list["result"]["structuredContent"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            list["result"]["structuredContent"]["sessions"][0]["id"],
            "claude:allowed"
        );

        let changed_request = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/roots/list_changed"
                })
                .to_string(),
            )
            .expect("server requests replacement roots"),
        );
        let hidden_uri = url::Url::from_directory_path(&hidden).unwrap().to_string();
        assert!(deliver_line(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": changed_request["id"],
                "result": { "roots": [{ "uri": hidden_uri }] }
            })
            .to_string(),
        )
        .is_none());
        let changed_list = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 4,
                    "method": "tools/call",
                    "params": { "name": "list_sessions", "arguments": {} }
                })
                .to_string(),
            )
            .unwrap(),
        );
        assert_eq!(
            changed_list["result"]["structuredContent"]["sessions"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            changed_list["result"]["structuredContent"]["sessions"][0]["id"],
            "claude:hidden"
        );

        let corrupt_request = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/roots/list_changed"
                })
                .to_string(),
            )
            .expect("server requests roots after another change"),
        );
        assert!(deliver_line(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": corrupt_request["id"],
                "result": { "roots": [{ "uri": "https://example.com/not-local" }] }
            })
            .to_string(),
        )
        .is_none());
        let corrupt_list = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 5,
                    "method": "tools/call",
                    "params": { "name": "list_sessions", "arguments": {} }
                })
                .to_string(),
            )
            .unwrap(),
        );
        assert!(corrupt_list["error"]["message"]
            .as_str()
            .unwrap()
            .contains("roots[0].uri must use the file scheme"));

        let schema = parse(
            &deliver_line(
                &mut server,
                &json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "tools/call",
                    "params": { "name": "query_session_index", "arguments": {} }
                })
                .to_string(),
            )
            .unwrap(),
        );
        assert!(schema["result"]["structuredContent"]["rows"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["name"] == "sessions"));
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
    fn every_enum_parameter_names_each_accepted_token_in_its_description() {
        // A caller binding an enum value reads the description to learn what each token
        // selects; a token present in the enum but absent from the description is invisible
        // to that caller (the shipped example: `field` described "one canonical tool
        // argument" in prose without naming the literal token `tool_argument`). Derive the
        // accepted-token list from the schema the dispatcher advertises, so this cannot
        // drift from a hand-written list.
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
                let description = spec["description"].as_str().unwrap_or_default();
                // An array parameter documents its member tokens on the parameter itself,
                // so check `items.enum` against the same description.
                for enum_values in [&spec["enum"], &spec["items"]["enum"]] {
                    let Some(tokens) = enum_values.as_array() else {
                        continue;
                    };
                    enums_checked += 1;
                    for token in tokens {
                        let token = token.as_str().unwrap();
                        assert!(
                            description.contains(token),
                            "{name}.{param}: accepted value `{token}` is missing from the \
                             description, so a caller reading the description cannot learn \
                             what it selects: {description}"
                        );
                    }
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
                    hit["policy_name"].as_str().unwrap(),
                    hit["category"].as_str().unwrap(),
                    hit["matched_text"].as_str().unwrap()
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
                .map(|hit| hit["content"].as_str().unwrap().to_string())
                .collect()
        };

        assert!(
            !contents(json!({}))
                .iter()
                .any(|text| text.contains("don't forget")),
            "an orchestrator prompt is not a human correction"
        );
        assert!(
            contents(json!({ "session_kinds": ["user", "subagent"] }))
                .iter()
                .any(|text| text.contains("don't forget")),
            "naming both classes must opt the spawned run back in -- nothing is lost, only \
             reclassified"
        );
        assert_eq!(
            contents(json!({ "session_kinds": ["subagent"] })),
            vec!["don't forget to run the tests"]
        );
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
            second["run"]["output"]["result"]["report"]["matches"][0]["matched_text"],
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
            tool["outputSchema"]["required"]
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
                && description.contains("Aise reads capability.toml only")
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
        for tool_name in ["search_sessions", "list_sessions", "search_messages"] {
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
            for tool_name in ["search_sessions", "list_sessions", "search_messages"] {
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
        assert_eq!(
            result_schema["properties"]["message_ref"]["additionalProperties"],
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
        assert!(search_messages["description"]
            .as_str()
            .is_some_and(|description| {
                description.contains("content")
                    && description.contains("tool_name")
                    && description.contains("tool_argument")
            }));
        assert!(
            search_messages["inputSchema"]["properties"]["tool_name_contains"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("tool_name"))
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
        assert_eq!(
            get_session_variants[0]["properties"]["session"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_variants[0]["properties"]["transcript"]["additionalProperties"],
            false
        );
        assert_eq!(
            get_session_variants[1]["properties"]["session_metadata"]["additionalProperties"],
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
                get_session_variants[2]["properties"][field]["items"]["additionalProperties"],
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
            "repair_commands",
            "index_update",
            "providers",
        ] {
            assert!(get_index_status["outputSchema"]["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == required)));
        }
        let index_update = &get_index_status["outputSchema"]["properties"]["index_update"];
        assert_eq!(index_update["additionalProperties"], false);
        assert_eq!(
            index_update["properties"]["state"]["enum"],
            json!(["in_progress", "attention_required"])
        );
        for internal in [
            "origin",
            "process_id",
            "schema_generation_before",
            "schema_generation_after",
            "files_seen",
            "sessions_updated",
        ] {
            assert!(index_update["properties"].get(internal).is_none());
        }
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
        for required in [
            "never changes matching, ordering, result count, context membership, or includes",
            "applies before field_view",
            "Conflicts with detail",
        ] {
            assert!(
                message_window.contains(required),
                "missing {required:?}: {message_window}"
            );
        }
        assert!(search_messages["description"]
            .as_str()
            .is_some_and(|d| d.contains("message_seq") && !d.contains("session_id, seq")));
        let match_mode = &search_messages["inputSchema"]["properties"]["query_mode"];
        assert_eq!(match_mode["enum"], json!(["literal", "regex", "fuzzy"]));
        assert_eq!(match_mode["default"], "literal");
        assert!(match_mode["description"].as_str().is_some_and(|d| {
            d.contains("Rust regex")
                && d.contains("bounded fuzzy")
                && d.contains("Defaults to literal")
        }));
        for (parameter, expected_default) in [
            ("all_results", "Defaults to false"),
            ("include_compaction", "Defaults to true"),
        ] {
            let description = search_messages["inputSchema"]["properties"][parameter]
                ["description"]
                .as_str()
                .unwrap();
            assert!(
                description.contains(expected_default),
                "{parameter} description omits {expected_default:?}: {description}"
            );
        }
        let receipt_description = search_messages["inputSchema"]["properties"]["receipt_level"]
            ["description"]
            .as_str()
            .unwrap();
        assert!(receipt_description.contains("summary includes planner diagnostics"));
        assert!(receipt_description.contains("full adds resolved parameter origins"));
        assert!(!receipt_description.contains("summary and full include"));
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
    fn invalid_tool_call_does_not_open_or_refresh_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("must-not-exist.db").display().to_string());
        let mut server = McpServer::new(config);

        let response = deliver_line(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "search_sessions",
                    "arguments": { "query": "x", "provder": "codex" }
                }
            })
            .to_string(),
        )
        .expect("request response");

        let response: Value = serde_json::from_str(&response).unwrap();
        assert_eq!(response["result"]["isError"], true);
        assert!(
            server.app.is_none(),
            "invalid calls must preserve lazy startup"
        );
        assert!(
            !dir.path().join("must-not-exist.db").exists(),
            "validation must not create an index"
        );
        assert!(!server.refresh_after_response);
        assert!(server.refresh_worker.handle.is_none());
    }

    #[test]
    fn cancellation_notification_is_fire_and_forget_and_does_not_open_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        let db_path = dir.path().join("must-not-exist.db");
        config.index.db_path = Some(db_path.display().to_string());
        let mut server = McpServer::new(config);

        let response = deliver_line(
            &mut server,
            &json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": 42, "reason": "test cancellation" }
            })
            .to_string(),
        );

        assert!(response.is_none());
        assert!(server.app.is_none());
        assert!(!db_path.exists());
    }

    #[test]
    fn four_independent_mcp_clients_read_the_same_page_concurrently() {
        let (dir, db) = fixture();
        drop(db);
        let mut config = config_for_fixture(&dir);
        config.index.refresh = crate::config::IndexRefresh::ExistingOnly;
        let request = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "search_messages",
                "arguments": { "query": "hello", "limit": 1, "offset": 0 }
            }
        })
        .to_string();

        let clients = (0..4)
            .map(|_| {
                let config = config.clone();
                let request = request.clone();
                std::thread::spawn(move || {
                    let mut server = McpServer::new(config);
                    deliver_line(
                        &mut server,
                        &json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}})
                            .to_string(),
                    )
                    .unwrap();
                    deliver_line(&mut server, &request).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let responses = clients
            .into_iter()
            .map(|client| client.join().unwrap())
            .collect::<Vec<_>>();

        assert!(responses.windows(2).all(|pair| pair[0] == pair[1]));
        let response = parse(&responses[0]);
        assert_eq!(
            response["result"]["structuredContent"]["results"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn auto_refresh_starts_only_after_the_tool_response_is_flushed() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config_for_fixture(&dir);
        config.providers.claude.enabled = false;
        config.providers.claude_desktop.enabled = false;
        config.providers.codex.enabled = false;
        config.providers.cursor.enabled = false;
        config.providers.antigravity.enabled = false;
        config.providers.pi.enabled = false;
        config.providers.aistudio.enabled = false;
        config.providers.gemini_cli.enabled = false;
        let mut server = McpServer::new(config);

        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": { "name": "get_index_status", "arguments": {} }
        })
        .to_string();
        let delivery_error = server
            .handle_line(&request, |_| anyhow::bail!("transport flush failed"))
            .unwrap_err();
        assert!(delivery_error
            .to_string()
            .contains("transport flush failed"));
        assert!(!server.refresh_after_response);
        assert!(server.refresh_worker.handle.is_none());

        let mut delivered = false;
        let produced_response = server
            .handle_line(&request, |_| {
                delivered = true;
                Ok::<(), anyhow::Error>(())
            })
            .unwrap();

        assert!(produced_response);
        assert!(delivered);
        assert!(!server.refresh_after_response);
        assert!(server.refresh_worker.handle.is_some());
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
                    json!({ "query": "hello", "provider": provider }),
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
            page["results"][0]["presentation"]["field_view"]["extent"]["additional_field_text"],
            "after"
        );

        let session =
            tool_get_session(&json!({ "session_id": "claude:test1" }), &config, &db).unwrap();
        assert!(session.contains("- Transcript lines returned: last 3"));
        assert!(!session.contains("transcript line 401"));
        assert!(session.contains("transcript line 402"));
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
