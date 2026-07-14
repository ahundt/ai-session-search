use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};

use crate::analysis_pipeline::AnalysisPolicySpec;
use crate::config::Config;
use crate::dates::{self, Bound};
use crate::db::Db;
use crate::inspect::InspectionOptions;
use crate::models::{MessageFilters, Provider, Role, SearchFilters, SessionMeta, SessionRecord};
use crate::refs::{extract_refs_from_text, ref_summary};
use crate::service::SessionSearch;
use crate::service::{AnalysisService, CatalogService, MessageService};
use crate::sql_query::{self, DbSchemaArgs, ResolvedDbQueryArgs};
use crate::util::{
    current_repo, normalize_path_prefix, render_posix_shell_command, resume_plan,
    select_message_lines, select_transcript_lines, truncate_for_display,
};

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
    advertised_tools: Option<Value>,
    refresh_worker: RefreshWorker,
    refresh_after_response: bool,
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
            advertised_tools: None,
            refresh_worker: RefreshWorker::default(),
            refresh_after_response: false,
        }
    }

    /// Process and deliver one newline-delimited JSON-RPC frame.
    ///
    /// `deliver` receives a serialized response and must return only after the transport has
    /// flushed it. Blank lines, malformed JSON, and notifications do not call `deliver` and return
    /// `false`; delivered requests return `true`. Automatic refresh starts only after successful
    /// delivery. Initialization is independent of transcript volume and index access.
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

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let response = match method {
            "initialize" => handle_initialize(id.clone()),
            "tools/list" => {
                let response = handle_tools_list(id.clone(), &self.config);
                self.advertised_tools = Some(response["result"]["tools"].clone());
                response
            }
            "tools/call" => match validate_tool_call(&params, self.advertised_tools()) {
                Err(err) => tool_error_response(id.clone(), err),
                Ok(()) => match open_mcp_app(&mut self.app, &self.config).and_then(|app| {
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
            "notifications/initialized" | "notifications/cancelled" => return Ok(None),
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
        .ok_or_else(|| format!("unknown tool: {tool_name}"))?;
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
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let object = value
                .as_object()
                .ok_or_else(|| invalid(format!("expected object, got {}", json_type(value))))?;
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
                        return Err(format!("unknown {tool_name} parameter at {path}: {key}"));
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
                .ok_or_else(|| invalid(format!("expected array, got {}", json_type(value))))?;
            if let Some(minimum) = schema.get("minItems").and_then(Value::as_u64) {
                if array.len() < minimum as usize {
                    return Err(invalid(format!("expected at least {minimum} items")));
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
            return Err(invalid(format!(
                "expected string, got {}",
                json_type(value)
            )));
        }
        Some("boolean") if !value.is_boolean() => {
            return Err(invalid(format!(
                "expected boolean, got {}",
                json_type(value)
            )));
        }
        Some("integer") if value.as_i64().is_none() && value.as_u64().is_none() => {
            return Err(invalid(format!(
                "expected integer, got {}",
                json_type(value)
            )));
        }
        Some("number") if !value.is_number() => {
            return Err(invalid(format!(
                "expected number, got {}",
                json_type(value)
            )));
        }
        Some(_) | None => {}
    }
    if let (Some(actual), Some(minimum)) = (
        value.as_f64(),
        schema.get("minimum").and_then(Value::as_f64),
    ) {
        if actual < minimum {
            return Err(invalid(format!("must be at least {minimum}")));
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            let choices = allowed
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(invalid(format!("must be one of {choices}")));
        }
    }
    Ok(())
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
) -> anyhow::Result<&'a SessionSearch> {
    if slot.is_none() {
        *slot = Some(SessionSearch::open(config.clone())?);
    }
    Ok(slot.as_ref().expect("application slot initialized above"))
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
    let app = match SessionSearch::open(config.clone()) {
        Ok(app) => app,
        Err(error) => {
            eprintln!(
                "aise mcp serve: background index refresh could not open the index: {error:#}"
            );
            return;
        }
    };
    if let Err(error) =
        crate::indexer::refresh_usable_index_nonblocking(config, app.database(), &|| {
            cancel.load(Ordering::Acquire)
        })
    {
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
                "name": "aise",
                // Single source of truth: the package version, never a hand-kept duplicate.
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": crate::mcp_install::agent_instructions()
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

fn get_session_output_schema() -> Value {
    json!({
        "type": "object",
        "oneOf": [
            {
                "properties": {
                    "session": { "type": "object", "additionalProperties": true },
                    "transcript": { "type": "object", "additionalProperties": true },
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
                    "session_metadata": { "type": "object", "additionalProperties": true },
                    "messages": { "type": "array", "items": { "type": "object", "additionalProperties": true } }
                },
                "required": ["session_id", "anchor_seq", "cwd", "repo", "title", "session_metadata", "messages"],
                "additionalProperties": false
            },
            {
                "properties": {
                    "session": { "type": "object", "additionalProperties": true },
                    "user_intent": { "type": "array" },
                    "tool_activity": { "type": "array" },
                    "refs": { "type": "array" },
                    "changed_files": { "type": "array" },
                    "time_profile": { "type": "object", "additionalProperties": true },
                    "next_commands": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["session", "user_intent", "tool_activity", "refs", "changed_files", "next_commands"],
                "additionalProperties": false
            }
        ]
    })
}

fn search_messages_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema_version": { "type": "integer" },
            "returned": { "type": "integer", "minimum": 0 },
            "next_offset": { "type": ["integer", "null"] },
            "pagination": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 0 },
                    "offset": { "type": "integer", "minimum": 0 },
                    "ordering": { "type": "string", "enum": ["session_id,seq"] }
                },
                "required": ["limit", "offset", "ordering"],
                "additionalProperties": false
            },
            "search_explain": { "type": ["object", "null"], "additionalProperties": true },
            "sessions": { "type": "object", "additionalProperties": { "type": "object", "additionalProperties": true } },
            "hits": { "type": "array", "items": { "type": "object", "additionalProperties": true } }
        },
        "required": ["schema_version", "returned", "next_offset", "pagination", "search_explain", "sessions", "hits"],
        "additionalProperties": false
    })
}

fn get_index_status_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "db_path": { "type": "string" },
            "parser_health": { "type": "object", "additionalProperties": true },
            "repairable_stale_sessions": { "type": "integer", "minimum": 0 },
            "unavailable_stale_sessions": { "type": "integer", "minimum": 0 },
            "repair_commands": { "type": "array", "items": { "type": "string" } },
            "providers": { "type": "array", "items": { "type": "object", "additionalProperties": true } }
        },
        "required": ["db_path", "parser_health", "repairable_stale_sessions", "unavailable_stale_sessions", "repair_commands", "providers"],
        "additionalProperties": false
    })
}

fn query_session_index_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "columns": { "type": "array", "items": { "type": "string" } },
            "rows": { "type": "array", "items": { "type": "object", "additionalProperties": true } },
            "row_truncated": { "type": "boolean" },
            "cells_truncated": { "type": "boolean" }
        },
        "required": ["columns", "rows", "row_truncated", "cells_truncated"],
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
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "search_sessions",
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
                            "since": {
                                "type": "string",
                                "description": "Lower time bound: sessions last updated at or after this. A date, duration, or relative time, e.g. '2026-01-15', '2026-01' (whole month), '202X' (whole decade), '7d' (last 7 days), 'yesterday'. Default: no lower bound."
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper time bound: sessions last updated at or before this. Same formats as 'since'. Default: no upper bound."
                            },
                            "when": {
                                "type": "string",
                                "description": "Single time span used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. Do not combine with since/until."
                            },
                            "limit": {
                                "type": "integer", "minimum": 0,
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response.", config.mcp.search_sessions_limit),
                                "default": config.mcp.search_sessions_limit
                            }
                        },
                        "required": ["query"],
                        "additionalProperties": false
                    }
                },
                {
                    "name": "get_session",
                    "description": format!("Return one session from {provider_summary} by ID or unique prefix. Use summary=true for compact evidence, transcript_lines=N for transcript text (0 returns all lines), or message_seq=N with context for one turn. Default returns {} transcript lines.", transcript_lines_default_label(config.mcp.get_session_transcript_lines)),
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
                                "description": "Return compact session summary/evidence: user intent, tool activity previews, refs, changed files, provenance, and follow-up commands. Mutually exclusive with transcript_lines and message_seq.",
                                "default": false
                            },
                            "include": { "type": "array", "items": { "type": "string", "enum": ["time_profile"] }, "description": "Optional bounded summary sections. Currently supports time_profile. Requires summary=true.", "default": [] },
                            "transcript_lines": {
                                "type": "integer",
                                "description": format!("Return transcript lines: positive=head, negative=tail, 0=entire transcript and may be very large. Bound this when skimming many sessions: a negative tail shows how a session ended, a positive head shows how it started, and 0 is for complete capture only. To pinpoint one turn, use search_messages and pass its message_seq here instead of reading a large window. Mutually exclusive with summary and message_seq. Default when no output selector is provided: {}.", config.mcp.get_session_transcript_lines),
                                "default": config.mcp.get_session_transcript_lines
                            },
                            "message_seq": {
                                "type": "integer", "minimum": 0,
                                "description": "Message sequence number copied from a search_messages hit. Returns a focused message-context result instead of transcript lines."
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
                                "description": format!("With message_seq: limit each returned message's displayed content (positive keeps its first N lines, negative keeps its last N lines, 0 keeps complete content; default {}). This presentation window does not change context membership or reference extraction. Use it to keep long tool output around one turn skimmable. Unlike transcript_lines, it never windows the whole session transcript.", config.mcp.lines_per_message),
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
                            "since": {
                                "type": "string",
                                "description": "Lower time bound: sessions last updated at or after this. A date, duration, or relative time, e.g. '2026-01-15', '202X' (whole decade), '7d' (last 7 days), 'yesterday'. Default: no lower bound."
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper time bound: sessions last updated at or before this. Same formats as 'since'. Default: no upper bound."
                            },
                            "when": {
                                "type": "string",
                                "description": "Single time span used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. Do not combine with since/until."
                            },
                            "limit": {
                                "type": "integer", "minimum": 0,
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response.", config.mcp.list_sessions_limit),
                                "default": config.mcp.list_sessions_limit
                            }
                        },
                        "additionalProperties": false
                    }
                },
                {
                    "name": "get_resume_command",
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
                    "description": "Search individual messages. Use provider to select one named session source. context=0 returns only hits; a positive context adds that many neighboring turns before and after each hit. Each hit includes a ready-to-call get_session request with message_seq.",
                    "outputSchema": search_messages_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Exact literal text to find in message content, case-insensitive. Punctuation is significant: '/goal' matches '/goal', not every 'goal'; '--path', 'C++', URLs, and file paths match literally. Provide query, regex, or fuzzy_query, not more than one." },
                            "regex": { "type": "string", "description": "Regular expression (Rust syntax) to match message content. Provide query, regex, or fuzzy_query, not more than one. Regex search uses aise's trigram prefilter when selective, then verifies matches with Rust regex." },
                            "fuzzy_query": { "type": "string", "description": "Approximate fuzzy text to find with nucleo matching. Explicit opt-in for remembered wording or typos. Use query for exact literal text and regex for patterns. Provide query, regex, or fuzzy_query, not more than one." },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"], "description": "Only this message role: user (non-command prompts), assistant, tool (tool calls/results), slash (human-entered commands such as /goal), or compaction. Omit for all roles." },
                            "kind": { "type": "string", "enum": ["conversation", "compaction", "tool_call", "tool_result", "unknown"], "description": "Only this semantic message kind. Use tool_call to search invocations without matching results." },
                            "field": { "type": "string", "enum": ["content", "tool_name", "tool_argument"], "description": "Search message content (default), tool names, or one canonical tool argument selected by argument_path.", "default": "content" },
                            "argument_path": { "type": "string", "description": "RFC 6901 JSON pointer relative to canonical tool-call args, e.g. '/cmd' or '/request/path'. Required only when field='tool_argument'." },
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "tool": { "type": "string", "description": "Only tool messages whose tool name contains this text (case-insensitive), e.g. 'edit', 'bash'. Omit for any tool." },
                            "session_id": { "type": "string", "description": "Exact session ID or unique prefix. Use this when chaining from search_messages/get_session results." },
                            "path_prefix": { "type": "string", "description": "Only messages from sessions whose working directory, git repo, or transcript path starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory." },
                            "exclude_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude messages from sessions whose working directory, git repo, or transcript path starts with any of these paths. Applied before limit/context. Omit for no path exclusions." },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" }, "description": "Exclude exact session IDs. Applied before limit/context. Omit for no session exclusions." },
                            "seq_from": { "type": "integer", "minimum": 0, "description": "Lower inclusive message sequence bound. Requires session_id because seq values are session-local." },
                            "seq_to": { "type": "integer", "minimum": 0, "description": "Upper inclusive message sequence bound. Requires session_id because seq values are session-local." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. A date, duration, or relative time, e.g. '2026-01-15', '202X' (whole decade), '7d' (last 7 days), 'yesterday'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound: messages at or before this. Same formats as 'since'. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single time span used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. Do not combine with since/until." },
                            "no_compaction": { "type": "boolean", "description": "Exclude auto-generated summary messages (default false).", "default": false },
                            "context": { "type": "integer", "minimum": 0, "description": "Return this many turns before and after each match in the same call (default 0). Use this for immediate one-step context.", "default": 0 },
                            "include_refs": { "type": "boolean", "description": "Include extracted URL-like references for returned hits and context rows (default false). Use with context for source audits.", "default": false },
                            "preview_chars": { "type": "integer", "minimum": 1, "description": format!("Maximum characters per concise hit/context preview (default {}). Ignored when response_format='detailed'.", config.mcp.preview_chars.max(1)), "default": config.mcp.preview_chars.max(1) },
                            "lines_per_message": { "type": "integer", "description": format!("Limit each hit's and context row's displayed content (positive keeps its first N lines, negative keeps its last N lines, 0 keeps complete content; default {}). This presentation window does not change matches, ranking, result count, pagination, context membership, or reference extraction. Use it to keep many hits or long tool outputs skimmable without discarding hits. It applies before preview_chars and, unlike get_session transcript_lines, never windows a whole session transcript.", config.mcp.lines_per_message), "default": config.mcp.lines_per_message },
                            "explain": { "type": "boolean", "description": "Include planner diagnostics for regex selectivity: corpus rows, trigram prefilter, candidate rows, and a concise tuning hint. Default false.", "default": false },
                            "limit": { "type": "integer", "minimum": 0, "description": format!("Maximum matching messages to return (default {}). Set 0 only to explicitly request all matching messages; next_offset is null for an unbounded result.", config.mcp.search_messages_limit.max(1)), "default": config.mcp.search_messages_limit.max(1) },
                            "offset": { "type": "integer", "minimum": 0, "description": "Skip this many matches before returning, to page through results (default 0).", "default": 0 },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "'concise' (default) trims each message to a snippet; 'detailed' returns full text.", "default": "concise" }
                        },
                        "additionalProperties": false
                    }
                },
                {
                    "name": "analyze_sessions",
                    "description": format!("Read-only classification, scoring, recurring-phrase aggregation, explicit relationship resolution, and graph projection over one selected AI session corpus. Rules are provider-neutral and optional; without rules the result still contains session metadata and cwd/repository groups. Selection is by canonical session ID ascending, with a default corpus limit of {}. Set limit=0 only to explicitly analyze every matching session, which can produce a large response. A bounded call is an independent corpus, not a mergeable result page; increase limit or narrow filters rather than combining graphs/vocabularies from separate calls. Use search_sessions/list_sessions first to discover a narrower corpus. This tool never publishes files.", config.mcp.analyze_sessions_limit),
                    "outputSchema": {
                        "type": "object",
                        "properties": {
                            "returned": { "type": "integer" },
                            "limit": { "type": "integer" },
                            "corpus_may_be_partial": { "type": "boolean" },
                            "selection_order": { "type": "string", "enum": ["session_id_asc"] },
                            "sessions": { "type": "object", "additionalProperties": true },
                            "vocabulary": { "type": "array", "items": { "type": "object", "additionalProperties": true } },
                            "graph": { "type": "object", "additionalProperties": true }
                        },
                        "required": ["returned", "limit", "corpus_may_be_partial", "selection_order", "sessions", "vocabulary", "graph"]
                    },
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "path_prefix": { "type": "string", "description": "Only sessions whose working directory, git repo, or transcript path starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory." },
                            "exclude_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude sessions under these path prefixes before selecting the analysis corpus." },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" }, "description": "Exclude these exact session IDs before selecting the analysis corpus." },
                            "since": { "type": "string", "description": "Lower session update-time bound using the shared CLI date grammar." },
                            "until": { "type": "string", "description": "Upper session update-time bound using the shared CLI date grammar." },
                            "when": { "type": "string", "description": "One session update-time span using the shared CLI date grammar. Do not combine with since/until." },
                            "limit": { "type": "integer", "minimum": 0, "description": format!("Maximum canonical-session-ID-ordered sessions in this independent analysis corpus (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Separate bounded calls are not mergeable pages.", config.mcp.analyze_sessions_limit), "default": config.mcp.analyze_sessions_limit },
                            "classification_rules": {
                                "type": "array",
                                "description": "Optional ordered classification rules. Rules are validated together; duplicate dimension+label pairs are rejected.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "dimension": { "type": "string" },
                                        "label": { "type": "string" },
                                        "target": { "type": "string", "enum": ["title", "summary", "first_user_text", "user_text", "any"] },
                                        "pattern": { "type": "string", "description": "Rust regular expression." },
                                        "weight": { "type": "integer" }
                                    },
                                    "required": ["dimension", "label", "target", "pattern", "weight"],
                                    "additionalProperties": false
                                },
                                "default": []
                            },
                            "relationship_rules": {
                                "type": "array",
                                "description": "Optional explicit name-derived relationship rules. Each Rust regex must contain a named capture (?P<parent>...). Shared cwd/repository values remain groups, never inferred lineage.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": { "type": "string" },
                                        "kind": { "type": "string", "enum": ["branch", "copy", "version"] },
                                        "pattern": { "type": "string", "description": "Rust regular expression containing a named parent capture." }
                                    },
                                    "required": ["id", "kind", "pattern"],
                                    "additionalProperties": false
                                },
                                "default": []
                            },
                            "phrase_vocabulary": {
                                "type": "object",
                                "description": "Optional recurring-phrase policy. Widths and max_unique_phrases are required explicit positive bounds.",
                                "properties": {
                                    "widths": { "type": "array", "minItems": 1, "items": { "type": "integer", "minimum": 1 } },
                                    "max_unique_phrases": { "type": "integer", "minimum": 1 },
                                    "min_document_tokens": { "type": "integer", "minimum": 0, "default": 0 },
                                    "excluded_tokens": { "type": "array", "items": { "type": "string" }, "default": [] },
                                    "exclude_numeric_tokens": { "type": "boolean", "default": true },
                                    "text_mode": { "type": "string", "enum": ["user_text", "prose_only"], "default": "user_text" }
                                },
                                "required": ["widths", "max_unique_phrases"],
                                "additionalProperties": false
                            },
                            "max_classification_chars": { "type": "integer", "minimum": 1, "description": "Optional explicit semantic window per classification rule. Omit to classify complete selected user text; this is not a memory tuning parameter." }
                        },
                        "additionalProperties": false
                    }
                },
                {
                    "name": "get_index_status",
                    "description": format!("Return index and parser status for {provider_summary}: current and stale session counts, parse warnings, discoverable sessions that can be reindexed, retained sessions whose source files are unavailable, and applicable repair commands. Equivalent to `aise doctor --format json`."),
                    "outputSchema": get_index_status_output_schema(),
                    "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
                },
                {
                    "name": "query_session_index",
                    "description": query_session_index_description,
                    "outputSchema": query_session_index_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "sql": { "type": "string", "description": "Exactly one raw read-only SQL statement returning rows from the local AI session-history index. Omit sql to list session-history schema objects. Prefer search_messages for accelerated content or regex search with context. Writes, ATTACH/DETACH, unsafe PRAGMAs, and multiple statements are rejected." },
                            "schema_table": { "type": "string", "description": "Optional table/view name for column details in the AI session-history index, such as sessions, messages, or file_edits. Use instead of sql." },
                            "include_internal": { "type": "boolean", "description": "When sql is omitted, include SQLite/FTS shadow tables and internal indexes for the session-history database (default false).", "default": false },
                            "limit": { "type": "integer", "minimum": 0, "description": format!("Maximum rows to return after the SQL statement runs (default {}). 0 means unlimited; prefer adding LIMIT in SQL for expensive queries.", config.db.query_limit), "default": config.db.query_limit },
                            "offset": { "type": "integer", "minimum": 0, "description": "Skip this many rows after the SQL statement runs (default 0). Prefer SQL LIMIT/OFFSET for expensive queries.", "default": 0 },
                            "timeout_ms": { "type": "integer", "minimum": 0, "description": format!("Interrupt the query after this many milliseconds (default {}). 0 disables the timeout.", config.db.query_timeout_ms), "default": config.db.query_timeout_ms },
                            "max_cell_chars": { "type": "integer", "minimum": 0, "description": format!("Maximum characters per string cell in the JSON response. 0 disables cell truncation. Default {}.", config.mcp.query_max_cell_chars), "default": config.mcp.query_max_cell_chars }
                        },
                        "additionalProperties": false
                    }
                }
            ]
        }
    })
}

fn handle_tools_call(id: Option<Value>, params: &Value, config: &Config, db: &Db) -> Value {
    let tool_name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "search_sessions" => tool_search_sessions(&args, config, db).map(ToolResponse::text),
        "get_session" => tool_get_session(&args, config, db),
        "list_sessions" => tool_list_sessions(&args, config, db).map(ToolResponse::text),
        "get_resume_command" => tool_get_resume_command(&args, db).map(ToolResponse::text),
        "search_messages" => tool_search_messages(&args, config, db),
        "analyze_sessions" => tool_analyze_sessions(&args, config, db),
        "get_index_status" => crate::diagnostics::collect(config, db)
            .map_err(|error| error.to_string())
            .and_then(|status| serde_json::to_value(status).map_err(|error| error.to_string()))
            .and_then(ToolResponse::structured),
        "query_session_index" => tool_query_session_index(&args, config),
        _ => Err(format!("unknown tool: {tool_name}")),
    };

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

fn reject_unknown_analysis_args(args: &Value) -> Result<(), String> {
    const ALLOWED: &[&str] = &[
        "provider",
        "path_prefix",
        "exclude_path_prefixes",
        "exclude_session_ids",
        "since",
        "until",
        "when",
        "limit",
        "classification_rules",
        "relationship_rules",
        "phrase_vocabulary",
        "max_classification_chars",
    ];
    let object = args
        .as_object()
        .ok_or_else(|| "analyze_sessions arguments must be an object".to_string())?;
    if let Some(key) = object.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(format!("unknown analyze_sessions parameter: {key}"));
    }
    Ok(())
}

fn analysis_policy_from_args(
    args: &Value,
) -> Result<crate::analysis_pipeline::AnalysisPolicy, String> {
    let policy: AnalysisPolicySpec = serde_json::from_value(json!({
        "classification_rules": args.get("classification_rules").cloned().unwrap_or_else(|| json!([])),
        "relationship_rules": args.get("relationship_rules").cloned().unwrap_or_else(|| json!([])),
        "phrase_vocabulary": args.get("phrase_vocabulary").cloned().unwrap_or(Value::Null),
        "max_classification_chars": args.get("max_classification_chars").cloned().unwrap_or(Value::Null),
    }))
    .map_err(|error| format!("invalid analyze_sessions policy: {error}"))?;
    policy
        .compile()
        .map_err(|error| format!("invalid analyze_sessions policy: {error}"))
}

fn tool_analyze_sessions(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    reject_unknown_analysis_args(args)?;
    let now = chrono::Utc::now();
    let filters = search_filters_from_args(args, config.mcp.analyze_sessions_limit, now)
        .map_err(|error| format!("invalid analyze_sessions filter: {error}"))?;
    let policy = analysis_policy_from_args(args)?;
    let result = AnalysisService::new(config, db)
        .run(&filters, &policy)
        .map_err(|error| error.to_string())?;
    let graph = result.session_graph();
    ToolResponse::structured(json!({
        "returned": result.sessions.len(),
        "limit": filters.limit,
        "corpus_may_be_partial": filters.limit != 0 && result.sessions.len() == filters.limit,
        "selection_order": "session_id_asc",
        "sessions": result.sessions,
        "vocabulary": result.vocabulary,
        "graph": graph,
    }))
}

#[derive(Debug)]
struct ToolResponse {
    text: String,
    structured_content: Option<Value>,
}

impl ToolResponse {
    fn text(text: String) -> Self {
        Self {
            text,
            structured_content: None,
        }
    }

    fn structured(value: Value) -> Result<Self, String> {
        let text = serde_json::to_string_pretty(&value).map_err(|err| err.to_string())?;
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

fn tool_search_sessions(args: &Value, config: &Config, db: &Db) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: query")?;
    let now = chrono::Utc::now();
    let filters = search_filters_from_args(args, config.mcp.search_sessions_limit, now)?;
    let repo = current_repo(config);
    let hits = CatalogService::new(db)
        .search_sessions(query, &filters, repo.as_deref(), &config.search.scoring)
        .map_err(|e| e.to_string())?;

    if hits.is_empty() {
        return Ok("No sessions found matching the query.".to_string());
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
    Ok(out)
}

fn tool_get_session(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;
    let summary = mcp_bool_arg(args, "summary", false);
    let message_seq = args.get("message_seq").and_then(Value::as_i64);
    let transcript_lines = args.get("transcript_lines").and_then(Value::as_i64);

    let selector_count =
        summary as usize + message_seq.is_some() as usize + transcript_lines.is_some() as usize;
    if selector_count > 1 {
        return Err(
            "Use only one get_session output selector: summary, transcript_lines, or message_seq."
                .to_string(),
        );
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
        let mut options = inspection_options_from_args(args, config);
        options.include_time_profile = include.iter().any(|value| value == "time_profile");
        let inspection = CatalogService::new(db)
            .inspect(session_id, options)
            .map_err(|e| e.to_string())?;
        return serde_json::to_value(&inspection)
            .map_err(|e| e.to_string())
            .and_then(ToolResponse::structured);
    }

    if let Some(seq) = message_seq {
        let session = db
            .resolve_session_record(session_id)
            .map_err(|e| e.to_string())?;
        let context = mcp_nonnegative_i64_arg(args, "context", 0);
        let presentation = MessagePresentation::from_args(args, config);
        return message_window_value(&session, seq, context, &presentation, db)
            .and_then(ToolResponse::structured);
    }
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

    let full = db.resolve_session(session_id).map_err(|e| e.to_string())?;
    let s = &full.session;

    let (transcript, returned_lines) =
        select_transcript_lines(&full.transcript_text, selected_lines);

    let title = s.title.as_deref().unwrap_or("(untitled)");
    let cwd = s.cwd.as_deref().unwrap_or("-");
    let updated = s
        .updated_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());

    let text = format!(
        "# {title}\n\n- ID: {}\n- Provider: {}\n- Provider Session ID: {}\n- CWD: {cwd}\n- Updated: {updated}\n- Messages: {}\n- Transcript lines returned: {returned_lines}\n\n## Transcript\n\n{transcript}",
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
                "lines_returned": returned_lines,
            },
            "rendered_text": text,
        }),
    ))
}

fn tool_list_sessions(args: &Value, config: &Config, db: &Db) -> Result<String, String> {
    let now = chrono::Utc::now();
    let filters = search_filters_from_args(args, config.mcp.list_sessions_limit, now)?;
    let sessions = CatalogService::new(db)
        .list_sessions(&filters)
        .map_err(|e| e.to_string())?;

    if sessions.is_empty() {
        return Ok("No sessions found.".to_string());
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
    Ok(out)
}

fn tool_get_resume_command(args: &Value, db: &Db) -> Result<String, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;

    let session = db
        .resolve_session_record(session_id)
        .map_err(|e| e.to_string())?;
    let (command, cwd) = resume_plan(&session).map_err(|e| e.to_string())?;

    let cmd_str = render_posix_shell_command(&command).map_err(|error| error.to_string())?;
    match cwd {
        Some(cwd) => {
            let change_dir = render_posix_shell_command(&["cd".to_string(), cwd])
                .map_err(|error| error.to_string())?;
            Ok(format!("{change_dir} && {cmd_str}"))
        }
        None => Ok(cmd_str),
    }
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
        let payload = sql_query::query_result_payload(&result, mcp_max_cell_chars(args, config));
        return ToolResponse::structured(payload.value);
    }

    let query_args = ResolvedDbQueryArgs {
        sql: sql.unwrap().to_string(),
        limit: mcp_usize_arg(args, "limit", config.db.query_limit),
        offset: mcp_usize_arg(args, "offset", 0),
        timeout_ms: mcp_u64_arg(args, "timeout_ms", config.db.query_timeout_ms),
        format: crate::render::OutputFormat::Json,
    };
    let result =
        sql_query::query_path(&config.db_path(), config.index.busy_timeout_ms, &query_args)
            .map_err(format_mcp_query_error)?;
    let payload = sql_query::query_result_payload(&result, mcp_max_cell_chars(args, config));
    ToolResponse::structured(payload.value)
}

fn format_mcp_query_error(err: anyhow::Error) -> String {
    sql_query::format_query_error(
        err,
        "query_session_index",
        "call query_session_index with no sql to list AI session-history tables, or schema_table to inspect columns",
    )
}

fn mcp_max_cell_chars(args: &Value, config: &Config) -> usize {
    mcp_usize_arg(args, "max_cell_chars", config.mcp.query_max_cell_chars)
}

fn mcp_bool_arg(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn mcp_u64_arg(args: &Value, key: &str, default: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default)
}

fn mcp_usize_arg(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|value| usize::try_from(value).unwrap_or_else(|_| max_mcp_numeric_usize()))
        .map(|value| value.min(max_mcp_numeric_usize()))
        .unwrap_or(default)
}

fn mcp_nonnegative_usize_arg(args: &Value, key: &str, default: usize) -> Result<usize, String> {
    let Some(value) = args.get(key) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| format!("{key} must be a non-negative integer"))?;
    usize::try_from(value).map_err(|_| format!("{key} is too large for this platform"))
}

fn max_mcp_numeric_usize() -> usize {
    usize::try_from(i64::MAX).unwrap_or(usize::MAX)
}

fn mcp_positive_usize_arg(args: &Value, key: &str, default: usize) -> usize {
    mcp_usize_arg(args, key, default).max(1)
}

fn inspection_options_from_args(args: &Value, config: &Config) -> InspectionOptions {
    InspectionOptions {
        preview_chars: mcp_positive_usize_arg(
            args,
            "preview_chars",
            config.mcp.preview_chars.max(1),
        ),
        include_time_profile: false,
    }
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

fn mcp_nonnegative_i64_arg(args: &Value, key: &str, default: i64) -> i64 {
    args.get(key)
        .and_then(Value::as_i64)
        .unwrap_or(default)
        .max(0)
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
        since,
        until,
        limit: mcp_nonnegative_usize_arg(args, "limit", default_limit)?,
        warnings_only: false,
    })
}

fn tool_search_messages(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let regex = args.get("regex").and_then(Value::as_str).map(String::from);
    let fuzzy_query = args
        .get("fuzzy_query")
        .and_then(Value::as_str)
        .map(String::from);
    let content_modes = [
        !query.is_empty(),
        regex.as_ref().is_some_and(|value| !value.is_empty()),
        fuzzy_query.as_ref().is_some_and(|value| !value.is_empty()),
    ]
    .into_iter()
    .filter(|enabled| *enabled)
    .count();
    if content_modes > 1 {
        return Err(
            "provide only one content search mode: query (exact literal), regex, or fuzzy_query"
                .to_string(),
        );
    }

    let now = chrono::Utc::now();
    // Omission uses a bounded default. Explicit zero is the shared unbounded sentinel.
    let limit = mcp_nonnegative_usize_arg(args, "limit", config.mcp.search_messages_limit.max(1))?;
    let offset = mcp_usize_arg(args, "offset", 0);
    // Neighbor counts are naturally bounded by the session length, so only clamp to non-negative.
    if args.get("session").is_some() {
        return Err(
            "unknown parameter `session`; use `session_id` with an exact ID or unique prefix"
                .to_string(),
        );
    }
    let context = mcp_nonnegative_i64_arg(args, "context", 0);
    let before = context;
    let after = context;
    let presentation = MessagePresentation::from_args(args, config);
    let include_refs = presentation.include_refs;

    let (since, until) = parse_date_bounds(args, now)?;
    let exact_session_arg = args.get("session_id").and_then(Value::as_str);
    let seq_from = args.get("seq_from").and_then(Value::as_i64);
    let seq_to = args.get("seq_to").and_then(Value::as_i64);
    if (seq_from.is_some() || seq_to.is_some()) && exact_session_arg.is_none() {
        return Err("seq_from/seq_to require session_id because seq is session-local".to_string());
    }
    if let (Some(from), Some(to)) = (seq_from, seq_to) {
        if from > to {
            return Err("seq_from must be <= seq_to".to_string());
        }
    }
    let catalog = CatalogService::new(db);
    let exact_session_id = exact_session_arg
        .map(|id| catalog.resolve_session(id).map(|session| session.id))
        .transpose()
        .map_err(|e| e.to_string())?;
    let filters = MessageFilters {
        role: parse_opt_enum::<Role>(args, "role")?,
        kind: parse_opt_enum::<crate::models::MessageKind>(args, "kind")?,
        field: parse_opt_enum::<crate::models::SearchField>(args, "field")?,
        argument_path: args
            .get("argument_path")
            .and_then(Value::as_str)
            .map(String::from),
        provider: parse_opt_enum::<Provider>(args, "provider")?,
        session_id: exact_session_id,
        path_prefix: args
            .get("path_prefix")
            .and_then(Value::as_str)
            .map(normalize_path_prefix),
        exclude_path_prefixes: parse_string_array(args, "exclude_path_prefixes")?
            .into_iter()
            .map(|path| normalize_path_prefix(&path))
            .collect(),
        exclude_session_ids: parse_string_array(args, "exclude_session_ids")?,
        since,
        until,
        seq_from,
        seq_to,
        regex,
        fuzzy_query,
        tool: args.get("tool").and_then(Value::as_str).map(String::from),
        no_compaction: mcp_bool_arg(args, "no_compaction", false),
        // Fetch one past a bounded page so next_offset is exact. Zero asks the service for all.
        limit: if limit == 0 {
            0
        } else {
            limit.saturating_add(1)
        },
        offset,
    };
    let include_explain = mcp_bool_arg(args, "explain", false);

    let messages = MessageService::new(db);
    let (mut hits, explain) = messages
        .search_with_explain(&query, &filters, include_explain)
        .map_err(|e| e.to_string())?;
    let explain = explain.map(|explain| {
        json!({
            "corpus": explain.corpus,
            "prefilter": explain.prefilter,
            "candidates": explain.candidates,
            "prefilter_skipped": explain.prefilter_skipped,
            "summary": explain.summary(filters.regex.is_some() || !query.is_empty() || filters.fuzzy_query.is_some()),
        })
    });
    let page_end = offset.saturating_add(limit);
    let has_more = limit != 0 && hits.len() > limit;
    let page: Vec<_> = if limit == 0 {
        hits
    } else {
        hits.drain(..).take(limit).collect()
    };
    let next_offset = has_more.then_some(page_end);

    // Enrich each hit with its session's cwd/repo/title in ONE batched lookup (no N+1).
    let mut ids: Vec<String> = page.iter().map(|h| h.session_id.clone()).collect();
    ids.sort();
    ids.dedup();
    let meta = messages.session_metadata(&ids).map_err(|e| e.to_string())?;

    let trim = |s: &str| presentation.trim(s);

    let hits_json: Vec<Value> = page
        .iter()
        .map(|h| {
            let m = meta.get(&h.session_id);
            let mut obj = json!({
                "session_id": h.session_id,
                "seq": h.seq,
                "role": h.role.as_str(),
                "kind": h.kind.as_str(),
                "provider": h.provider.as_str(),
                "ts": h.ts.map(|t| t.to_rfc3339()),
                "tool_name": h.tool_name,
                "tool_call_id": h.tool_call_id,
                "cwd": m.and_then(|m| m.cwd.clone()),
                "repo": m.and_then(|m| m.repo_root.clone()),
                "title": m.and_then(|m| m.title.clone()),
                "content": trim(&h.content),
                "context_request": {
                    "tool": "get_session",
                    "arguments": {
                        "session_id": h.session_id,
                        "message_seq": h.seq,
                        "context": 5
                    }
                },
            });
            if let Some(score) = h.fuzzy_score {
                obj["match_mode"] = json!("fuzzy");
                obj["fuzzy_score"] = json!(score);
            }
            if include_refs {
                let refs = extract_refs_from_text(&h.content, h.tool_name.as_deref());
                obj["ref_summary"] = json!(ref_summary(&refs));
                obj["refs"] = json!(refs);
            }
            if before > 0 || after > 0 {
                if let Ok(ctx) = db.message_context(&h.session_id, h.seq, before, after) {
                    let rows: Vec<Value> = ctx
                        .iter()
                        .map(|c| {
                            let mut row = json!({
                                "seq": c.seq,
                                "role": c.role.as_str(),
                                "kind": c.kind.as_str(),
                                "provider": c.provider.as_str(),
                                "ts": c.ts.map(|t| t.to_rfc3339()),
                                "tool_name": c.tool_name,
                                "tool_call_id": c.tool_call_id,
                                "is_match": c.seq == h.seq,
                                "session_id": h.session_id,
                                "content": trim(&c.content),
                            });
                            if include_refs {
                                let refs =
                                    extract_refs_from_text(&c.content, c.tool_name.as_deref());
                                row["ref_summary"] = json!(ref_summary(&refs));
                                row["refs"] = json!(refs);
                            }
                            row
                        })
                        .collect();
                    obj["context"] = Value::Array(rows);
                }
            }
            obj
        })
        .collect();

    let out = json!({
        "schema_version": crate::db::SCHEMA_VERSION,
        "returned": hits_json.len(),
        "next_offset": next_offset,
        "pagination": {
            "limit": limit,
            "offset": offset,
            "ordering": "session_id,seq"
        },
        "search_explain": explain,
        "sessions": meta
            .iter()
            .map(|(id, meta)| (id.clone(), session_meta_json(meta)))
            .collect::<serde_json::Map<String, Value>>(),
        "hits": hits_json,
    });
    ToolResponse::structured(out)
}

/// How message content is shaped for one response: full or concise preview, optional refs,
/// and the per-message line cap. Parsed once per tool call from the shared argument names.
struct MessagePresentation {
    detailed: bool,
    include_refs: bool,
    preview_chars: usize,
    lines_per_message: i64,
}

impl MessagePresentation {
    fn from_args(args: &Value, config: &Config) -> Self {
        Self {
            detailed: args.get("response_format").and_then(Value::as_str) == Some("detailed"),
            include_refs: mcp_bool_arg(args, "include_refs", false),
            preview_chars: mcp_positive_usize_arg(
                args,
                "preview_chars",
                config.mcp.preview_chars.max(1),
            ),
            lines_per_message: mcp_i64_arg(args, "lines_per_message", config.mcp.lines_per_message),
        }
    }

    /// Per-message line cap first (head/tail selection), then concise char preview if requested.
    /// Refs are always extracted from full content so a cap never hides references.
    fn trim(&self, content: &str) -> String {
        let capped = select_message_lines(content, self.lines_per_message);
        if self.detailed {
            capped
        } else {
            truncate_for_display(&capped, self.preview_chars)
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
        .map_err(|e| e.to_string())?;
    let include_refs = presentation.include_refs;
    let trim = |s: &str| presentation.trim(s);
    let messages: Vec<Value> = rows
        .iter()
        .map(|c| {
            let mut row = json!({
                "seq": c.seq,
                "role": c.role.as_str(),
                "kind": c.kind.as_str(),
                "provider": c.provider.as_str(),
                "ts": c.ts.map(|t| t.to_rfc3339()),
                "tool_name": c.tool_name,
                "tool_call_id": c.tool_call_id,
                "is_match": c.seq == seq,
                "content": trim(&c.content),
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

fn session_meta_json(meta: &SessionMeta) -> Value {
    let mut out = serde_json::Map::new();
    insert_string(
        &mut out,
        "provider_session_id",
        meta.provider_session_id.as_deref(),
    );
    insert_string(&mut out, "cwd", meta.cwd.as_deref());
    insert_string(&mut out, "repo", meta.repo_root.as_deref());
    insert_string(&mut out, "title", meta.title.as_deref());
    insert_time(&mut out, "updated_at", meta.updated_at);
    insert_time(&mut out, "last_message_at", meta.last_message_at);
    if let Some(count) = meta.message_count {
        out.insert("message_count".to_string(), json!(count));
    }
    insert_string(&mut out, "parse_warning", meta.parse_warning.as_deref());
    Value::Object(out)
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
    use crate::models::Message;
    use crate::util::minimal_record;
    use std::path::Path;

    /// A temp index holding one session (rooted at `/Users/x/proj`) with three messages,
    /// built entirely through the public API so these tests exercise the real persist path.
    fn fixture() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let mut parsed = minimal_record(Provider::Claude, Path::new("/x/s.jsonl"), String::new());
        parsed.session.id = "claude:test1".to_string();
        parsed.session.provider_session_id = "test1".to_string();
        parsed.session.cwd = Some("/Users/x/proj".to_string());
        parsed.session.repo_root = Some("/Users/x/proj".to_string());
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

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
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

    fn config_for_fixture(dir: &tempfile::TempDir) -> Config {
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        config
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum MessageSearchMode {
        Exact,
        Regex,
        Fuzzy,
    }

    impl MessageSearchMode {
        fn args(self, pattern: &str) -> Value {
            match self {
                Self::Exact => json!({ "query": pattern }),
                Self::Regex => json!({ "regex": pattern }),
                Self::Fuzzy => json!({ "fuzzy_query": pattern }),
            }
        }
    }

    const MESSAGE_SEARCH_MODE_CASES: [(MessageSearchMode, &str); 3] = [
        (MessageSearchMode::Exact, "hello"),
        (MessageSearchMode::Regex, "h.llo"),
        (MessageSearchMode::Fuzzy, "helo"),
    ];

    fn with_search_mode(mut args: Value, mode: MessageSearchMode, pattern: &str) -> Value {
        let map = args.as_object_mut().expect("test args must be an object");
        for (key, value) in mode.args(pattern).as_object().unwrap() {
            map.insert(key.clone(), value.clone());
        }
        args
    }

    #[test]
    fn search_messages_enriches_with_session_metadata_and_paginates() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        // "hello" matches the two user turns; each hit is enriched with the session's
        // cwd/repo/title (the agent-facing context) and carries session_id+seq for chaining.
        let out = parse(&tool_search_messages(&json!({ "query": "hello" }), &config, &db).unwrap());
        assert_eq!(out["returned"], 2);
        assert!(out["next_offset"].is_null());
        let hit = &out["hits"][0];
        assert_eq!(hit["session_id"], "claude:test1");
        assert_eq!(hit["cwd"], "/Users/x/proj");
        assert_eq!(hit["repo"], "/Users/x/proj");
        assert_eq!(hit["title"], "Proj");
        let session_meta = &out["sessions"]["claude:test1"];
        assert_eq!(session_meta["provider_session_id"], "test1");
        assert_eq!(session_meta["cwd"], "/Users/x/proj");
        assert_eq!(session_meta["repo"], "/Users/x/proj");
        assert_eq!(session_meta["title"], "Proj");
        assert_eq!(session_meta["message_count"], 3);
        assert!(
            session_meta.get("source_path").is_none(),
            "search pages keep ingestion provenance out of repeated metadata"
        );
        assert_eq!(hit["context_request"]["tool"], "get_session");
        assert_eq!(
            hit["context_request"]["arguments"]["session_id"],
            "claude:test1"
        );
        assert!(hit["context_request"]["arguments"]["message_seq"].is_number());

        // Page size 1: the first page reports a next_offset, the last page reports none.
        let p0 = parse(
            &tool_search_messages(
                &json!({ "query": "hello", "limit": 1, "offset": 0 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(p0["returned"], 1);
        assert_eq!(p0["next_offset"], 1);
        let p1 = parse(
            &tool_search_messages(
                &json!({ "query": "hello", "limit": 1, "offset": 1 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(p1["returned"], 1);
        assert!(p1["next_offset"].is_null());
    }

    #[test]
    fn search_messages_explain_reports_regex_planner_diagnostics() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_search_messages(
                &json!({
                    "regex": "hello",
                    "explain": true,
                    "limit": 1
                }),
                &config,
                &db,
            )
            .unwrap(),
        );

        let explain = &out["search_explain"];
        assert!(explain["corpus"].as_i64().unwrap() >= 1);
        assert!(explain["prefilter"].as_str().unwrap().contains("hel"));
        assert!(explain["candidates"].as_i64().unwrap() >= 1);
        assert!(explain["summary"]
            .as_str()
            .unwrap()
            .contains("trigram prefilter"));
    }

    #[test]
    fn search_messages_path_filter_context_window_and_mutual_exclusion() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        for (mode, pattern) in MESSAGE_SEARCH_MODE_CASES {
            // A path_prefix not containing the session filters it out entirely.
            let none = parse(
                &tool_search_messages(
                    &with_search_mode(json!({ "path_prefix": "/Users/x/other" }), mode, pattern),
                    &config,
                    &db,
                )
                .unwrap(),
            );
            assert_eq!(
                none["returned"], 0,
                "{mode:?}: path prefix excludes session"
            );

            // A matching absolute path_prefix returns the session's user messages. The fixture cwd
            // does not exist on disk, so this also exercises the lexical-absolute fallback path.
            let scoped = parse(
                &tool_search_messages(
                    &with_search_mode(
                        json!({ "path_prefix": "/Users/x/proj", "role": "user" }),
                        mode,
                        pattern,
                    ),
                    &config,
                    &db,
                )
                .unwrap(),
            );
            assert_eq!(
                scoped["returned"], 2,
                "{mode:?}: path prefix includes session"
            );
            let hit = &scoped["hits"][0];
            assert_eq!(hit["cwd"], "/Users/x/proj");
            assert_eq!(hit["repo"], "/Users/x/proj");
            assert_eq!(scoped["sessions"]["claude:test1"]["title"], "Proj");
            if mode == MessageSearchMode::Fuzzy {
                assert_eq!(hit["match_mode"], "fuzzy");
                assert!(hit["fuzzy_score"].as_u64().unwrap() > 0);
            }
        }

        // context is the simple one-step path: symmetric before/after turns are attached
        // in the search response, with the match row flagged.
        let ctx = parse(
            &tool_search_messages(&json!({ "query": "alpha", "context": 1 }), &config, &db)
                .unwrap(),
        );
        let window = ctx["hits"][0]["context"].as_array().expect("context array");
        assert!(window
            .iter()
            .any(|m| m["is_match"] == true && m["seq"] == 0));
        assert!(
            window.iter().any(|m| m["seq"] == 1),
            "includes the next turn"
        );
        assert_eq!(window[0]["provider"], "claude");

        // Passing both `query` and `regex` is a clear error, not a silent precedence.
        assert!(
            tool_search_messages(&json!({ "query": "a", "regex": "b" }), &config, &db).is_err()
        );
        assert!(tool_search_messages(
            &json!({ "query": "hello", "fuzzy_query": "helo" }),
            &config,
            &db
        )
        .is_err());
        assert!(tool_search_messages(
            &json!({ "regex": "hello", "fuzzy_query": "helo" }),
            &config,
            &db
        )
        .is_err());
    }

    #[test]
    fn search_messages_supports_fuzzy_query_with_scores() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_search_messages(
                &json!({
                    "fuzzy_query": "helo",
                    "role": "user",
                    "limit": 2,
                    "explain": true
                }),
                &config,
                &db,
            )
            .unwrap(),
        );

        assert_eq!(out["returned"], 2);
        let hit = &out["hits"][0];
        assert_eq!(hit["match_mode"], "fuzzy");
        assert!(hit["fuzzy_score"].as_u64().unwrap() > 0);
        assert!(hit["content"].as_str().unwrap().contains("hello"));
        assert!(out["search_explain"]["summary"]
            .as_str()
            .unwrap()
            .contains("nucleo fuzzy scorer"));
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

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "hello",
                    "session_id": "claude:test1",
                    "seq_from": 1,
                    "seq_to": 2
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["returned"], 1);
        assert_eq!(out["hits"][0]["seq"], 2);

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

    #[test]
    fn search_messages_include_refs_adds_structured_url_refs() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "beta",
                    "include_refs": true,
                    "response_format": "detailed"
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        let hit = &out["hits"][0];
        assert_eq!(hit["ref_summary"], "url");
        assert_eq!(hit["refs"][0]["value"], "https://example.com/paper.pdf");
        assert_eq!(hit["refs"][0]["host"], "example.com");

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
            "2026-01-31T23:59:59+00:00"
        );

        let (since, until) = parse_date_bounds(&json!({ "when": "2026-01" }), now).unwrap();
        assert_eq!(since.unwrap().to_rfc3339(), "2026-01-01T00:00:00+00:00");
        assert_eq!(until.unwrap().to_rfc3339(), "2026-01-31T23:59:59+00:00");
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
                "path_prefix": "/Users/x/proj/.",
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
            Some(normalize_path_prefix("/Users/x/proj/."))
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
        assert_eq!(out["cwd"], "/Users/x/proj");
        assert_eq!(out["repo"], "/Users/x/proj");
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

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "needle",
                    "response_format": "detailed",
                    "include_refs": true,
                    "lines_per_message": 2
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        let hits = out["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0]["content"], "needle first line\nsecond line",
            "positive lines_per_message keeps the head of each hit"
        );
        let refs = hits[0]["refs"].as_array().unwrap();
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
        assert_eq!(out["cells_truncated"], true);
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
        assert_eq!(r["serverInfo"]["name"], "aise");
        assert!(r["capabilities"]["tools"].is_object());
        assert_eq!(r["instructions"], crate::mcp_install::agent_instructions());
        assert!(r["instructions"].as_str().unwrap().chars().count() <= 512);
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
        assert!(provider["resume_supported"].is_boolean());
        assert_eq!(status["repair_commands"][0], "aise reindex --full");
    }

    #[test]
    fn analyze_sessions_reuses_typed_policy_and_preserves_explicit_unbounded_limit() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let arguments = json!({
            "limit": 1,
            "classification_rules": [{
                "dimension": "topic",
                "label": "greeting",
                "target": "user_text",
                "pattern": "(?i)\\bhello\\b",
                "weight": 3
            }],
            "phrase_vocabulary": {
                "widths": [1],
                "max_unique_phrases": 20,
                "exclude_numeric_tokens": true,
                "text_mode": "user_text"
            }
        });
        let response = call_tool("analyze_sessions", arguments.clone(), &config, &db);
        let result = &response["result"]["structuredContent"];

        assert_eq!(result["returned"], 1);
        assert_eq!(result["limit"], 1);
        assert_eq!(result["corpus_may_be_partial"], true);
        assert_eq!(result["selection_order"], "session_id_asc");
        assert_eq!(result["sessions"]["claude:test1"]["score"], 3);
        assert_eq!(
            result["sessions"]["claude:test1"]["classifications"][0]["label"],
            "greeting"
        );
        assert_eq!(result["graph"]["nodes"]["claude:test1"]["score"], 3);
        let hello = result["vocabulary"]
            .as_array()
            .unwrap()
            .iter()
            .find(|phrase| phrase["phrase"] == "hello")
            .expect("hello vocabulary entry");
        assert_eq!(hello["documents"], 1);
        assert_eq!(hello["occurrences"], 2);

        let mut unbounded = arguments;
        unbounded["limit"] = json!(0);
        let unbounded_response = call_tool("analyze_sessions", unbounded, &config, &db);
        assert_eq!(
            unbounded_response["result"]["structuredContent"]["limit"],
            0
        );
        assert_eq!(
            unbounded_response["result"]["structuredContent"]["corpus_may_be_partial"],
            false
        );

        for invalid in [
            json!({ "output_dir": "not-allowed" }),
            json!({
                "classification_rules": [{
                    "dimension": "topic",
                    "label": "greeting",
                    "target": "user_text",
                    "pattern": "hello",
                    "weight": 1,
                    "weigth": 99
                }]
            }),
            json!({
                "phrase_vocabulary": { "widths": [1], "max_unique_phrases": 0 }
            }),
            json!({ "limit": -1 }),
        ] {
            let rejected = call_tool("analyze_sessions", invalid, &config, &db);
            assert_eq!(rejected["result"]["isError"], true, "{rejected}");
            assert!(rejected["result"]["content"][0]["text"]
                .as_str()
                .is_some_and(|text| text.contains("analyze_sessions")));
        }
    }

    #[test]
    fn tools_list_exposes_expected_tools_each_with_a_schema() {
        let (dir, _db) = fixture();
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
                "analyze_sessions",
                "get_index_status",
                "query_session_index",
            ]
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
        for tool_name in [
            "search_sessions",
            "list_sessions",
            "search_messages",
            "analyze_sessions",
        ] {
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
        for tool_name in ["search_sessions", "list_sessions", "analyze_sessions"] {
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
            for tool_name in [
                "search_sessions",
                "list_sessions",
                "search_messages",
                "analyze_sessions",
            ] {
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
        let analyze_sessions = tools
            .iter()
            .find(|tool| tool["name"] == "analyze_sessions")
            .expect("analyze_sessions advertised");
        for tool in [
            get_session,
            search_messages,
            analyze_sessions,
            query_session_index,
        ] {
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
        assert!(get_session["outputSchema"]["oneOf"]
            .as_array()
            .is_some_and(|variants| variants
                .iter()
                .all(|variant| variant["additionalProperties"] == false)));
        let get_index_status = tools
            .iter()
            .find(|tool| tool["name"] == "get_index_status")
            .expect("get_index_status advertised");
        assert_eq!(
            get_index_status["outputSchema"]["additionalProperties"],
            false
        );
        for required in [
            "db_path",
            "parser_health",
            "repairable_stale_sessions",
            "unavailable_stale_sessions",
            "repair_commands",
            "providers",
        ] {
            assert!(get_index_status["outputSchema"]["required"]
                .as_array()
                .is_some_and(|fields| fields.iter().any(|field| field == required)));
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
            "does not change matches, ranking, result count, pagination, context membership, or reference extraction",
            "without discarding hits",
            "never windows a whole session transcript",
        ] {
            assert!(message_window.contains(required), "missing {required:?}: {message_window}");
        }
        assert!(search_messages["description"]
            .as_str()
            .is_some_and(|d| d.contains("message_seq") && !d.contains("session_id, seq")));
        assert_eq!(
            search_messages["inputSchema"]["properties"]["explain"]["default"], false,
            "planner diagnostics are opt-in"
        );
        assert!(
            search_messages["inputSchema"]["properties"]["regex"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("trigram prefilter"))
        );
        assert!(
            search_messages["inputSchema"]["properties"]["fuzzy_query"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("nucleo") && d.contains("query for exact"))
        );
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
                json!({ "query": "x", "preview_chars": 0 }),
                "must be at least 1",
            ),
            (
                "analyze_sessions",
                json!({ "limit": -1 }),
                "must be at least 0",
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
    fn mcp_json_tools_return_structured_content_matching_text_json() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        for (tool, arguments) in [
            ("search_messages", json!({ "query": "hello", "limit": 1 })),
            ("query_session_index", json!({ "schema_table": "messages" })),
            ("analyze_sessions", json!({ "limit": 1 })),
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
    }

    #[test]
    fn mcp_config_controls_advertised_and_runtime_defaults() {
        let (dir, db) = fixture();
        let mut config = config_for_fixture(&dir);
        config.mcp.search_sessions_limit = 7;
        config.mcp.list_sessions_limit = 8;
        config.mcp.analyze_sessions_limit = 6;
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
        let analyze_sessions = tools
            .iter()
            .find(|t| t["name"] == "analyze_sessions")
            .expect("analyze_sessions advertised");
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
            analyze_sessions["inputSchema"]["properties"]["limit"]["default"],
            6
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
            search_messages["inputSchema"]["properties"]["preview_chars"]["default"],
            10
        );
        assert!(get_session["description"]
            .as_str()
            .is_some_and(|d| d.contains("last 3 transcript lines")));

        let page =
            parse(&tool_search_messages(&json!({ "query": "hello" }), &config, &db).unwrap());
        assert_eq!(page["returned"], 1);
        assert_eq!(page["next_offset"], 1);
        assert_eq!(page["hits"][0]["content"], "alpha h...");

        let session =
            tool_get_session(&json!({ "session_id": "claude:test1" }), &config, &db).unwrap();
        assert!(session.contains("- Transcript lines returned: last 3"));
        assert!(!session.contains("transcript line 401"));
        assert!(session.contains("transcript line 402"));
    }

    #[test]
    fn search_messages_explicit_zero_returns_all_matches_without_a_next_offset() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let bounded = parse(&tool_search_messages(&json!({ "limit": 1 }), &config, &db).unwrap());
        assert_eq!(bounded["returned"], 1);
        assert_eq!(bounded["next_offset"], 1);

        let unbounded = parse(&tool_search_messages(&json!({ "limit": 0 }), &config, &db).unwrap());
        assert!(unbounded["returned"]
            .as_u64()
            .is_some_and(|count| count > 1));
        assert!(unbounded["next_offset"].is_null());
        assert_eq!(unbounded["pagination"]["limit"], 0);
    }
}
