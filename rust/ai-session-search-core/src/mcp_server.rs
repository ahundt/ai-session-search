use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};

use serde_json::{json, Value};

use crate::config::Config;
use crate::dates::{self, Bound};
use crate::db::Db;
use crate::inspect::InspectionOptions;
use crate::models::{
    MessageFilters, MessageSearchMode, Provider, Role, SearchFilters, SessionMeta, SessionRecord,
};
use crate::refs::{extract_refs_from_text, ref_summary};
use crate::service::SessionSearch;
use crate::service::{CatalogService, MessageService};
use crate::sql_query::{self, DbSchemaArgs, ResolvedDbQueryArgs};
use crate::util::{
    current_repo, normalize_path_prefix, render_posix_shell_command, resume_plan,
    select_message_lines, select_transcript_lines, truncate_for_display,
};

/// Context radius in the generated one-call `get_session` continuation for a message hit.
const GET_SESSION_FOLLOW_UP_CONTEXT: i64 = 5;

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
            // Cancellation is an optional MCP utility. This synchronous stdio implementation has
            // no in-flight request registry, so it may ignore cancellation for a request that
            // cannot be interrupted, as the specification permits. Closing stdin or terminating
            // the child remains the transport-level cancellation/cleanup path. Never respond to
            // either notification.
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

/// Tool annotations shared by every tool this server exposes. All of them only
/// read the local session index: they search, list, fetch, or run read-only SQL
/// and never mutate provider files or the index. `readOnlyHint` lets clients
/// skip the destructive-action confirmation they otherwise assume for a tool
/// with no annotations, and `openWorldHint: false` states the domain is the
/// closed local index rather than an open external world.
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
                            "lines_returned": { "type": "string" }
                        },
                        "required": ["text", "lines_returned"],
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
            "discovery_source": { "type": "string" }
        },
        "required": [
            "id", "provider", "provider_session_id", "title", "summary", "cwd", "repo_root",
            "created_at", "updated_at", "last_message_at", "preview_text", "source_path",
            "message_count", "parse_version", "raw_metadata_json", "parse_warning",
            "discovery_source"
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
            "returned": { "type": "integer", "minimum": 0, "description": "Number of sessions returned after the limit." }
        },
        "required": ["sessions", "returned"],
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
            "returned": { "type": "integer", "minimum": 0, "description": "Number of sessions returned after the limit." }
        },
        "required": ["sessions", "returned"],
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
            "is_match"
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
        json!({ "type": "string", "enum": ["conversation", "compaction", "tool_call", "tool_result", "unknown"] }),
    );
    properties.insert("provider".into(), provider_id_output_schema());
    properties.insert("ts".into(), json!({ "type": ["string", "null"] }));
    properties.insert("tool_name".into(), json!({ "type": ["string", "null"] }));
    properties.insert("tool_call_id".into(), json!({ "type": ["string", "null"] }));
    properties.insert("content".into(), json!({ "type": "string" }));
    properties.insert("ref_summary".into(), json!({ "type": "string" }));
    properties.insert(
        "refs".into(),
        json!({ "type": "array", "items": message_reference_output_schema() }),
    );
    properties
}

fn message_context_row_output_schema() -> Value {
    let mut properties = message_row_properties();
    properties.insert("is_match".into(), json!({ "type": "boolean" }));
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "session_id", "seq", "role", "kind", "provider", "ts", "tool_name",
            "tool_call_id", "content", "is_match"
        ],
        "additionalProperties": false
    })
}

fn message_context_request_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "tool": { "type": "string", "enum": ["get_session"] },
            "arguments": {
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "message_seq": { "type": "integer", "minimum": 0 },
                    "context": { "type": "integer", "minimum": 0 }
                },
                "required": ["session_id", "message_seq", "context"],
                "additionalProperties": false
            }
        },
        "required": ["tool", "arguments"],
        "additionalProperties": false
    })
}

fn message_hit_output_schema() -> Value {
    let mut properties = message_row_properties();
    properties.insert("cwd".into(), json!({ "type": ["string", "null"] }));
    properties.insert("repo".into(), json!({ "type": ["string", "null"] }));
    properties.insert("title".into(), json!({ "type": ["string", "null"] }));
    properties.insert(
        "context_request".into(),
        message_context_request_output_schema(),
    );
    properties.insert(
        "match_mode".into(),
        json!({ "type": "string", "enum": ["fuzzy"] }),
    );
    properties.insert("fuzzy_score".into(), json!({ "type": "number" }));
    properties.insert(
        "context".into(),
        json!({ "type": "array", "items": message_context_row_output_schema() }),
    );
    json!({
        "type": "object",
        "properties": properties,
        "required": [
            "session_id", "seq", "role", "kind", "provider", "ts", "tool_name",
            "tool_call_id", "cwd", "repo", "title", "content", "context_request"
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

fn search_explain_output_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "properties": {
            "corpus": { "type": "integer", "minimum": 0 },
            "prefilter": { "type": ["string", "null"] },
            "candidates": { "type": ["integer", "null"], "minimum": 0 },
            "prefilter_skipped": { "type": ["string", "null"] },
            "candidate_source_saturated": { "type": "boolean", "description": "True when an indexed candidate source exceeded its bounded admission budget; narrow structural filters for better fuzzy recall." },
            "summary": { "type": "string" }
        },
        "required": ["corpus", "prefilter", "candidates", "prefilter_skipped", "candidate_source_saturated", "summary"],
        "additionalProperties": false
    })
}

fn search_messages_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "schema_version": { "type": "integer", "description": "Version of this search_messages response contract." },
            "match_mode": { "type": "string", "enum": ["exact", "regex", "fuzzy"], "description": "Effective interpretation of query for this complete page." },
            "returned": { "type": "integer", "minimum": 0, "description": "Number of matching messages in this response page." },
            "next_offset": { "type": ["integer", "null"], "minimum": 0, "description": "Offset for the next non-overlapping page, or null when no matching messages remain." },
            "pagination": {
                "type": "object",
                "description": "Effective page request and deterministic result order.",
                "properties": {
                    "limit": { "type": "integer", "minimum": 0, "description": "Maximum matching messages requested; 0 means all exact/regex matches, while fuzzy requires a positive finite limit." },
                    "offset": { "type": "integer", "minimum": 0, "description": "Matching messages skipped before this page." },
                    "ordering": { "type": "string", "enum": ["session_id,seq", "fuzzy_score desc,exact_phrase desc,session_id,seq"], "description": "Deterministic order used for non-overlapping offset pages; fuzzy ranks by score and exact-phrase tie preference before stable identity." }
                },
                "required": ["limit", "offset", "ordering"],
                "additionalProperties": false
            },
            "search_explain": search_explain_output_schema(),
            "sessions": { "type": "object", "description": "Session metadata keyed by the exact session_id values referenced by hits and context rows.", "additionalProperties": session_meta_output_schema() },
            "hits": { "type": "array", "description": "Matching messages after filters, offset, and limit, each with requested context and a get_session continuation.", "items": message_hit_output_schema() }
        },
        "required": ["schema_version", "match_mode", "returned", "next_offset", "pagination", "search_explain", "sessions", "hits"],
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
        "required": ["db_path", "parser_health", "repairable_stale_sessions", "unavailable_stale_sessions", "repair_commands", "index_update", "providers"],
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
            "stale_sessions": { "type": "integer", "minimum": 0 },
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
            "repairable_stale_sessions", "unavailable_stale_sessions", "resume_command"
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
    json!({
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
                                "type": "integer", "minimum": 0,
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Accepts a positive count or 0.", config.mcp.search_sessions_limit),
                                "default": config.mcp.search_sessions_limit
                            }
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
                            "preview_chars": { "type": "integer", "minimum": 1, "description": format!("Maximum characters per concise message/tool/ref preview in summary output and focused message context (default {}). Not used for transcript output.", config.mcp.preview_chars.max(1)), "default": config.mcp.preview_chars.max(1) },
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
                                "type": "integer", "minimum": 0,
                                "description": format!("Maximum sessions to return (default {}). Set 0 only to explicitly request all matching sessions; this can produce a large response. Accepts a positive count or 0.", config.mcp.list_sessions_limit),
                                "default": config.mcp.list_sessions_limit
                            }
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
                    "description": "Search individual messages. Use provider to select one named session source. context=0 returns only hits; a positive context adds that many neighboring turns before and after each hit. Identify a returned message by the pair (session_id, message_seq): the hit's sequence field is seq, while its ready-to-call get_session request supplies message_seq. Hits also name role, kind, provider, tool_name, tool_call_id, and content. To read more of one session, continue from the next seq range (seq_from = last returned seq + 1) rather than re-requesting with a larger limit, which re-sends messages you already received.",
                    "outputSchema": search_messages_output_schema(),
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Text or pattern to find. Omit or pass an empty string only with match_mode='exact' to list messages selected by the other filters. With exact matching, comparison is case-insensitive and punctuation is significant: '/goal' matches '/goal', not every 'goal'; '--path', 'C++', URLs, and file paths match literally." },
                            "match_mode": { "type": "string", "enum": ["exact", "regex", "fuzzy"], "description": "How to interpret query: exact (default) is a case-insensitive literal substring and supports short or unlimited results; regex uses Rust regex syntax and requires a non-empty query; fuzzy finds remembered wording or typos and requires at least 3 characters plus a finite non-zero limit.", "default": "exact" },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"], "description": "Only this message role: user (non-command prompts), assistant, tool (tool calls/results), slash (human-entered commands such as /goal), or compaction. Omit for all roles." },
                            "kind": { "type": "string", "enum": ["conversation", "compaction", "tool_call", "tool_result", "unknown"], "description": "Only this semantic message kind: conversation (ordinary user/assistant turns), compaction (auto-generated summary messages), tool_call (a tool invocation, matched without its result), tool_result (the output a tool returned), or unknown (a message whose kind could not be classified). Omit for all kinds." },
                            "field": { "type": "string", "enum": ["content", "tool_name", "tool_argument"], "description": "Select the field searched by query: content (default), the canonical tool_name, or tool_argument for one canonical tool argument selected by argument_path.", "default": "content" },
                            "argument_path": { "type": "string", "description": "RFC 6901 JSON pointer relative to canonical tool-call args, e.g. '/cmd' or '/request/path'. Required only when field='tool_argument'." },
                            "provider": provider_filter_schema(&provider_values, &provider_filter_description),
                            "tool": { "type": "string", "description": "Additionally require the canonical tool_name to contain this text (case-insensitive), e.g. 'edit' or 'bash'. This filter is independent of the field searched by query; omit it to allow any tool_name." },
                            "session_id": { "type": "string", "description": "Exact session ID or unique prefix. Use this when chaining from search_messages/get_session results." },
                            "path_prefix": { "type": "string", "description": "Only messages from sessions whose working directory, git repo, or transcript path starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory." },
                            "exclude_path_prefixes": { "type": "array", "items": { "type": "string" }, "description": "Exclude messages from sessions whose working directory, git repo, or transcript path starts with any of these paths. Applied before limit/context. Omit for no path exclusions." },
                            "exclude_session_ids": { "type": "array", "items": { "type": "string" }, "description": "Exclude exact session IDs. Applied before limit/context. Omit for no session exclusions." },
                            "seq_from": { "type": "integer", "minimum": 0, "description": "Lower inclusive message sequence bound. Requires session_id because seq values are session-local. Pair with seq_to to read one session in non-overlapping chunks (e.g. 0..499, then 500..999) without re-reading turns." },
                            "seq_to": { "type": "integer", "minimum": 0, "description": "Upper inclusive message sequence bound. Requires session_id because seq values are session-local. See seq_from for non-overlapping chunked reads." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. Calendar/relative periods use UTC; an exact RFC 3339 timestamp honors Z or its explicit offset and preserves fractional seconds. Examples: '2026-01-15', '202X', '7d', 'yesterday', '2026-01-15T14:30:25.123Z'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound, inclusive: messages at or before this. Same precision and timezone rules as since. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single UTC period used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. An exact RFC 3339 value selects that instant at its stated precision. Do not combine with since/until." },
                            "no_compaction": { "type": "boolean", "description": "Exclude auto-generated summary messages (default false).", "default": false },
                            "context": { "type": "integer", "minimum": 0, "description": "Return this many turns before and after each match in the same call (default 0). Use this for immediate one-step context.", "default": 0 },
                            "include_refs": { "type": "boolean", "description": "Include extracted URL-like references for returned hits and context rows (default false). Use with context for source audits.", "default": false },
                            "preview_chars": { "type": "integer", "minimum": 1, "description": format!("Maximum characters per concise hit/context preview (default {}). Ignored when response_format='detailed'.", config.mcp.preview_chars.max(1)), "default": config.mcp.preview_chars.max(1) },
                            "lines_per_message": { "type": "integer", "description": format!("Limit each hit's and context row's displayed content (positive keeps its first N lines, negative keeps its last N lines, 0 keeps complete content; default {}). This presentation window does not change matches, ranking, result count, pagination, context membership, or reference extraction. Use it to keep many hits or long tool outputs skimmable without discarding hits. It applies before preview_chars and bounds each hit on its own; use get_session transcript_lines to window a whole session transcript.", config.mcp.lines_per_message), "default": config.mcp.lines_per_message },
                            "explain": { "type": "boolean", "description": "Include the canonical planner receipt for exact, regex, or fuzzy search: structurally filtered corpus rows, indexed prefilter, candidate rows, whether the prefilter was skipped, whether a bounded fuzzy candidate source saturated, and a concise tuning hint. Default false.", "default": false },
                            "limit": { "type": "integer", "minimum": 0, "description": format!("Maximum matching messages to return (default {}). Hits are ordered oldest-first (ordering=session_id,seq), so limit keeps the EARLIEST N, not the newest; to read the most recent N of one session, pass order=newest with session_id, or bound seq_from/seq_to, or page with offset. Exact and regex modes may set 0 to explicitly request every match — with no narrowing filters that is the entire index in one response, so prefer filters or paging; fuzzy mode requires a finite non-zero limit and offset + limit <= 10,000. next_offset is null for an unbounded exact/regex result. Accepts a positive count or 0; lines_per_message takes negatives for the last N lines.", config.mcp.search_messages_limit.max(1)), "default": config.mcp.search_messages_limit.max(1) },
                            "offset": { "type": "integer", "minimum": 0, "description": "Skip this many matches before returning, to page through results (default 0). Accepts a positive count or 0.", "default": 0 },
                            "order": { "type": "string", "enum": ["oldest", "newest", "relevance"], "description": "Result ordering. oldest (default for exact/regex) keeps the EARLIEST matches by seq; newest keeps the LAST N by seq and returns them oldest-first for readable transcripts, and requires session_id because seq numbers are session-local; relevance (default for fuzzy) ranks by fuzzy score. Omit to use the per-mode default." },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "'concise' (default) trims each message to a snippet; 'detailed' returns full text.", "default": "concise" }
                        },
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
        "search_sessions" => tool_search_sessions(&args, config, db),
        "get_session" => tool_get_session(&args, config, db),
        "list_sessions" => tool_list_sessions(&args, config, db),
        "get_resume_command" => tool_get_resume_command(&args, db),
        "search_messages" => tool_search_messages(&args, config, db),
        "get_index_status" => crate::diagnostics::collect(config, db)
            .map_err(|error| error.to_string())
            .and_then(|status| serde_json::to_value(status).map_err(|error| error.to_string()))
            .and_then(ToolResponse::structured),
        "query_session_index" => tool_query_session_index(&args, config),
        // Derive the served names from the advertised list rather than restating them, so this
        // recovery hint can never drift from what tools/list actually publishes.
        _ => Err(unknown_tool_message(
            tool_name,
            &handle_tools_list(None, config)["result"]["tools"],
        )),
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

#[derive(Debug)]
struct ToolResponse {
    text: String,
    structured_content: Option<Value>,
}

impl ToolResponse {
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

fn tool_search_sessions(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
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

    // Structured output mirrors `aise search --format json` (an array of flattened
    // SearchHit records) so MCP and CLI consumers see the same element shape; the text
    // stays a compact human-readable digest via structured_with_text.
    let structured = json!({
        "sessions": serde_json::to_value(&hits).map_err(|e| e.to_string())?,
        "returned": hits.len(),
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

    if has_range {
        reject_non_default(
            args,
            "context",
            json!(0),
            "context only applies with message_seq; a seq_from/seq_to range reads every message in [seq_from, seq_to]",
        )?;
        let session = db
            .resolve_session_record(session_id)
            .map_err(|e| e.to_string())?;
        let presentation = MessagePresentation::from_args(args, config);
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

fn tool_list_sessions(args: &Value, config: &Config, db: &Db) -> Result<ToolResponse, String> {
    let now = chrono::Utc::now();
    let filters = search_filters_from_args(args, config.mcp.list_sessions_limit, now)?;
    let sessions = CatalogService::new(db)
        .list_sessions(&filters)
        .map_err(|e| e.to_string())?;

    // Structured output mirrors `aise list --format json` (an array of session records).
    let structured = json!({
        "sessions": serde_json::to_value(&sessions).map_err(|e| e.to_string())?,
        "returned": sessions.len(),
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
        .map_err(|e| e.to_string())?;
    let (command, cwd) = resume_plan(&session).map_err(|e| e.to_string())?;

    let cmd_str = render_posix_shell_command(&command).map_err(|error| error.to_string())?;
    // The text is the ready-to-run command; structured output names the resolved session
    // and working directory so a caller can resume programmatically without parsing prose.
    let (resume_command, cwd_value) = match cwd {
        Some(cwd) => {
            let change_dir = render_posix_shell_command(&["cd".to_string(), cwd.clone()])
                .map_err(|error| error.to_string())?;
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
        let payload = sql_query::query_result_payload(&result, 0, mcp_max_cell_chars(args, config));
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
    let payload = sql_query::query_result_payload(
        &result,
        query_args.offset,
        mcp_max_cell_chars(args, config),
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

fn inspection_options_from_args(
    args: &Value,
    config: &Config,
) -> Result<InspectionOptions, String> {
    Ok(InspectionOptions {
        preview_chars: mcp_positive_usize_arg(
            args,
            "preview_chars",
            config.mcp.preview_chars.max(1),
        ),
        evidence_window: crate::inspect::EvidenceWindow::from_signed_items(
            args.get("summary_items")
                .and_then(Value::as_i64)
                .unwrap_or(config.mcp.summary_items),
        )
        .map_err(|error| error.to_string())?,
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

fn mcp_nonnegative_i64_arg(args: &Value, key: &str, default: i64) -> i64 {
    args.get(key)
        .and_then(Value::as_i64)
        .unwrap_or(default)
        .max(0)
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
    let match_mode = args
        .get("match_mode")
        .and_then(Value::as_str)
        .unwrap_or("exact")
        .parse::<MessageSearchMode>()?;

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
            return Err(format!(
                "seq_from must be <= seq_to, got {from} > {to}; \
                 swap the bounds or raise seq_to to at least {from}"
            ));
        }
    }
    // `order` is an explicit selection axis, never a sign on `limit` (see
    // notes/2026_07_20_2015_read_windowing_naming_web_research_and_decision.md, D1). `oldest`
    // and `relevance` keep the existing seq-ascending / fuzzy-ranked path; `newest` selects the
    // last N by seq and is only defined for one session because seq numbers are session-local.
    let newest_order = match args.get("order").and_then(Value::as_str) {
        None | Some("oldest") | Some("relevance") => false,
        Some("newest") => true,
        Some(other) => {
            return Err(format!(
                "order must be one of \"oldest\", \"newest\", \"relevance\"; got {other:?}"
            ))
        }
    };
    if newest_order && exact_session_arg.is_none() {
        return Err("order=newest requires session_id because seq is session-local".to_string());
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
        match_mode,
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
    // The newest-N read path (search_messages_ordered) does not surface a planner receipt, so
    // reject the combination rather than silently drop a requested explain.
    if newest_order && include_explain {
        return Err(
            "explain is not available with order=newest; drop explain or use the default order"
                .to_string(),
        );
    }

    let messages = MessageService::new(db);
    let (mut hits, explain) = if newest_order {
        // Select the last N by seq via the ordered DB path; the rows come back seq-descending and
        // are restored to chronological order below (avoids the git `--reverse`-after-limit trap).
        let hits = db
            .search_messages_ordered(&query, &filters, crate::db::MessageOrder::NewestFirst)
            .map_err(|e| e.to_string())?;
        (hits, None)
    } else {
        messages
            .search_with_explain(&query, &filters, include_explain)
            .map_err(|e| e.to_string())?
    };
    let explain = explain.map(|explain| {
        json!({
            "corpus": explain.corpus,
            "prefilter": explain.prefilter,
            "candidates": explain.candidates,
            "prefilter_skipped": explain.prefilter_skipped,
            "candidate_source_saturated": explain.candidate_source_saturated,
            "summary": explain.summary(!query.is_empty()),
        })
    });
    let page_end = offset.saturating_add(limit);
    let has_more = limit != 0 && hits.len() > limit;
    // For newest, `hits` are seq-descending, so take() keeps the newest `limit` before the extra
    // look-ahead row; reversing afterwards presents them oldest-first for readable transcripts.
    let mut page: Vec<_> = if limit == 0 {
        hits
    } else {
        hits.drain(..).take(limit).collect()
    };
    if newest_order {
        page.reverse();
    }
    // TODO(nextCursor, D2b): emit an opaque `nextCursor` (base64 of the offset) and accept it back
    // as `cursor` per the MCP pagination vocabulary. Deferred to keep RC scope bounded — the
    // deterministic seq_from/seq_to range read plus the forward-paging guidance in the tool
    // descriptions already give non-overlapping reads, so this is an ergonomics upgrade, not a
    // blocker. See notes/2026_07_20_2015_read_windowing_naming_web_research_and_decision.md, D2b.
    let next_offset = has_more.then_some(page_end);

    // Enrich each hit with its session's cwd/repo/title in ONE batched lookup (no N+1).
    let mut ids: Vec<String> = page.iter().map(|h| h.session_id.clone()).collect();
    ids.sort();
    ids.dedup();
    let meta = messages.session_metadata(&ids).map_err(|e| e.to_string())?;

    let trim = |s: &str| presentation.trim(s);

    let hits_json: Vec<Value> = page
        .iter()
        .map(|h| -> Result<Value, String> {
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
                        "context": GET_SESSION_FOLLOW_UP_CONTEXT
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
                // Propagate a failed context lookup instead of silently omitting the
                // `context` key: a caller who asked for context and receives a hit without
                // one cannot distinguish "no neighbors" from "the read failed".
                {
                    let ctx = db
                        .message_context(&h.session_id, h.seq, before, after)
                        .map_err(|e| e.to_string())?;
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
            Ok(obj)
        })
        .collect::<Result<_, _>>()?;

    let out = json!({
        "schema_version": crate::db::SCHEMA_VERSION,
        "match_mode": match_mode.as_str(),
        "returned": hits_json.len(),
        "next_offset": next_offset,
        "pagination": {
            "limit": limit,
            "offset": offset,
            "ordering": if match_mode == MessageSearchMode::Fuzzy {
                "fuzzy_score desc,exact_phrase desc,session_id,seq"
            } else {
                "session_id,seq"
            }
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
                "is_match": seq_from == Some(c.seq),
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
        "anchor_seq": seq_from.unwrap_or(0),
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

    const MESSAGE_SEARCH_MODE_CASES: [(MessageSearchMode, &str); 3] = [
        (MessageSearchMode::Exact, "hello"),
        (MessageSearchMode::Regex, "h.llo"),
        (MessageSearchMode::Fuzzy, "helo"),
    ];

    fn with_search_mode(mut args: Value, mode: MessageSearchMode, pattern: &str) -> Value {
        let map = args.as_object_mut().expect("test args must be an object");
        map.insert("query".to_string(), json!(pattern));
        if mode != MessageSearchMode::Exact {
            map.insert("match_mode".to_string(), json!(mode.as_str()));
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
        assert_eq!(hit["cwd"], FIXTURE_PROJECT);
        assert_eq!(hit["repo"], FIXTURE_PROJECT);
        assert_eq!(hit["title"], "Proj");
        let session_meta = &out["sessions"]["claude:test1"];
        assert_eq!(session_meta["provider_session_id"], "test1");
        assert_eq!(session_meta["cwd"], FIXTURE_PROJECT);
        assert_eq!(session_meta["repo"], FIXTURE_PROJECT);
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
        assert_eq!(
            hit["context_request"]["arguments"]["context"],
            GET_SESSION_FOLLOW_UP_CONTEXT
        );

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
    fn search_messages_runtime_fields_are_declared_by_the_output_schema() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let schema = search_messages_output_schema();
        let hit_properties = schema["properties"]["hits"]["items"]["properties"]
            .as_object()
            .expect("hit schema properties");
        let context_properties = hit_properties["context"]["items"]["properties"]
            .as_object()
            .expect("context schema properties");
        let reference_properties = hit_properties["refs"]["items"]["properties"]
            .as_object()
            .expect("reference schema properties");

        for args in [
            json!({ "query": "hello" }),
            json!({ "query": "helo", "match_mode": "fuzzy" }),
            json!({ "query": "alpha", "context": 1, "include_refs": true }),
        ] {
            let output = parse(&tool_search_messages(&args, &config, &db).unwrap());
            for hit in output["hits"].as_array().expect("runtime hits") {
                for field in hit.as_object().expect("runtime hit").keys() {
                    assert!(
                        hit_properties.contains_key(field),
                        "runtime hit field {field} is absent from outputSchema"
                    );
                }
                for row in hit["context"].as_array().into_iter().flatten() {
                    for field in row.as_object().expect("runtime context row").keys() {
                        assert!(
                            context_properties.contains_key(field),
                            "runtime context field {field} is absent from outputSchema"
                        );
                    }
                }
                for reference in hit["refs"].as_array().into_iter().flatten() {
                    for field in reference.as_object().expect("runtime reference").keys() {
                        assert!(
                            reference_properties.contains_key(field),
                            "runtime reference field {field} is absent from outputSchema"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn search_messages_explain_reports_regex_planner_diagnostics() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "hello",
                    "match_mode": "regex",
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
                    &with_search_mode(
                        json!({ "path_prefix": FIXTURE_OTHER_PROJECT }),
                        mode,
                        pattern,
                    ),
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
                        json!({ "path_prefix": FIXTURE_PROJECT, "role": "user" }),
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
            assert_eq!(hit["cwd"], FIXTURE_PROJECT);
            assert_eq!(hit["repo"], FIXTURE_PROJECT);
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

        // Non-exact modes require a query, and mode values are closed and explicit.
        assert!(tool_search_messages(&json!({ "match_mode": "regex" }), &config, &db).is_err());
        assert!(tool_search_messages(&json!({ "match_mode": "fuzzy" }), &config, &db).is_err());
        assert!(tool_search_messages(
            &json!({ "query": "hello", "match_mode": "approximate" }),
            &config,
            &db
        )
        .is_err());
    }

    #[test]
    fn search_messages_supports_fuzzy_matching_with_scores() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "helo",
                    "match_mode": "fuzzy",
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
            .contains("SQLite word FTS + trigram-overlap union"));
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
            ("content", "exact", "cargo test"),
            ("content", "regex", r"cargo\s+test"),
            ("content", "fuzzy", "crgo tst"),
            ("tool_name", "exact", "exec"),
            ("tool_name", "regex", r"^exec_"),
            ("tool_name", "fuzzy", "excmd"),
            ("tool_argument", "exact", "cargo test"),
            ("tool_argument", "regex", r"cargo\s+test"),
            ("tool_argument", "fuzzy", "crgo tst"),
        ];
        for (field, mode, query) in cases {
            let mut args = json!({
                "query": query,
                "field": field,
                "match_mode": mode,
                "kind": "tool_call",
                "session_id": "claude:matrix",
                "limit": 10,
                "explain": true
            });
            if field == "tool_argument" {
                args["argument_path"] = json!("/cmd");
            }
            let out = parse(
                &tool_search_messages(&args, &config, &db)
                    .unwrap_or_else(|error| panic!("{field}/{mode}: {error}")),
            );
            assert_eq!(out["returned"], 1, "{field}/{mode}: {out}");
            assert_eq!(out["hits"][0]["session_id"], "claude:matrix");
            assert_eq!(out["hits"][0]["seq"], 0);
            assert_eq!(out["match_mode"], mode);
            if mode == "fuzzy" {
                assert_eq!(out["hits"][0]["match_mode"], "fuzzy");
                assert_eq!(
                    out["pagination"]["ordering"],
                    "fuzzy_score desc,exact_phrase desc,session_id,seq"
                );
            } else {
                assert!(out["hits"][0].get("match_mode").is_none());
                assert_eq!(out["pagination"]["ordering"], "session_id,seq");
            }
            assert!(out["search_explain"].is_object());
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
        let out = parse(
            &tool_search_messages(
                &json!({
                    "session_id": "claude:test1",
                    "order": "newest",
                    "limit": 2
                }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["returned"], 2, "{out}");
        assert_eq!(out["hits"][0]["seq"], 1);
        assert_eq!(out["hits"][1]["seq"], 2);

        // limit 1 = the single most recent message (seq 2), not the earliest.
        let last = parse(
            &tool_search_messages(
                &json!({ "session_id": "claude:test1", "order": "newest", "limit": 1 }),
                &config,
                &db,
            )
            .unwrap(),
        );
        assert_eq!(last["returned"], 1);
        assert_eq!(last["hits"][0]["seq"], 2);
    }

    #[test]
    fn search_messages_order_newest_requires_session_id() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        // seq numbers are session-local, so newest is undefined without a single session scope.
        let error = tool_search_messages(&json!({ "order": "newest" }), &config, &db)
            .expect_err("newest without session_id must be rejected");
        assert!(
            error.contains("session_id") && error.contains("session-local"),
            "error names the missing session scope and why: {error}"
        );

        // explain has no planner receipt on the ordered read path; the combination is rejected
        // rather than silently dropping the requested receipt.
        let explain_error = tool_search_messages(
            &json!({ "session_id": "claude:test1", "order": "newest", "explain": true }),
            &config,
            &db,
        )
        .expect_err("newest + explain must be rejected");
        assert!(explain_error.contains("explain"), "{explain_error}");

        // an unknown order value names the accepted set.
        let bad = tool_search_messages(
            &json!({ "session_id": "claude:test1", "order": "sideways" }),
            &config,
            &db,
        )
        .expect_err("unknown order must be rejected");
        assert!(bad.contains("newest") && bad.contains("relevance"), "{bad}");
    }

    #[test]
    fn search_messages_schema_documents_order_and_forward_paging() {
        let (dir, _db) = fixture();
        let config = config_for_fixture(&dir);
        let tool = tool_input_schema(&config, "search_messages");

        let order = &tool["inputSchema"]["properties"]["order"];
        assert_eq!(
            order["enum"],
            json!(["oldest", "newest", "relevance"]),
            "order advertises the three selection values"
        );
        let order_doc = order["description"].as_str().unwrap();
        assert!(
            order_doc.contains("requires session_id") && order_doc.contains("session-local"),
            "order doc states newest needs a session scope and why: {order_doc}"
        );

        // Anti-pattern guidance (task 35): the tool description tells callers to advance the seq
        // range instead of re-requesting a larger limit.
        let tool_doc = tool["description"].as_str().unwrap();
        assert!(
            tool_doc.contains("seq_from = last returned seq + 1"),
            "search_messages description advertises forward-paging: {tool_doc}"
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
        assert!(provider["resume_command"].is_null() || provider["resume_command"].is_string());
        assert_eq!(status["repair_commands"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn every_advertised_tool_declares_read_only_annotations_and_an_output_schema() {
        // Every tool this server exposes only reads the local index and returns a structured
        // result. Assert both invariants over the WHOLE advertised list (not a per-tool spot
        // check) so a future tool that forgets an annotation or an outputSchema fails here, and
        // clients never assume a destructive default or an opaque, unschematized result.
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
                "{name} advertises readOnlyHint=true so clients skip destructive-action gating"
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
        assert_eq!(structured["sessions"][0]["id"], "claude:test1");
        // Text digest is preserved and is not the JSON blob.
        let text = result["content"][0]["text"].as_str().expect("text content");
        assert!(text.contains("claude:test1"));
        assert!(serde_json::from_str::<Value>(text).is_err());
    }

    #[test]
    fn get_resume_command_structured_command_matches_text() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);
        let response = call_tool(
            "get_resume_command",
            json!({ "session_id": "claude:test1" }),
            &config,
            &db,
        );
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
        let hit_schema = &search_messages["outputSchema"]["properties"]["hits"]["items"];
        assert_eq!(
            hit_schema["additionalProperties"], false,
            "search_messages hits must advertise every runtime field"
        );
        for field in [
            "session_id",
            "seq",
            "role",
            "kind",
            "provider",
            "ts",
            "tool_name",
            "tool_call_id",
            "cwd",
            "repo",
            "title",
            "content",
            "context_request",
            "match_mode",
            "fuzzy_score",
            "ref_summary",
            "refs",
            "context",
        ] {
            assert!(
                hit_schema["properties"].get(field).is_some(),
                "search_messages hit schema must document {field}"
            );
        }
        assert_eq!(
            hit_schema["properties"]["context_request"]["additionalProperties"],
            false
        );
        assert_eq!(
            hit_schema["properties"]["context"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            hit_schema["properties"]["refs"]["items"]["additionalProperties"],
            false
        );
        assert_eq!(
            hit_schema["properties"]["refs"]["items"]["properties"]["normalized_value"]["type"],
            json!(["string", "null"])
        );
        assert!(search_messages["description"]
            .as_str()
            .is_some_and(|description| description.contains("tool_name")));
        assert!(
            search_messages["inputSchema"]["properties"]["tool"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("tool_name"))
        );
        assert_eq!(
            search_messages["outputSchema"]["properties"]["search_explain"]["additionalProperties"],
            false
        );
        assert_eq!(
            search_messages["outputSchema"]["properties"]["sessions"]["additionalProperties"]
                ["additionalProperties"],
            false
        );
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
            "does not change matches, ranking, result count, pagination, context membership, or reference extraction",
            "without discarding hits",
            "bounds each hit on its own",
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
        let match_mode = &search_messages["inputSchema"]["properties"]["match_mode"];
        assert_eq!(match_mode["enum"], json!(["exact", "regex", "fuzzy"]));
        assert_eq!(match_mode["default"], "exact");
        assert!(match_mode["description"]
            .as_str()
            .is_some_and(|d| d.contains("Rust regex")
                && d.contains("at least 3 characters")
                && d.contains("finite non-zero limit")));
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
                json!({ "query": "x", "preview_chars": 0 }),
                "must be at least 1",
            ),
            ("search_messages", json!({ "regex": "x" }), "unknown"),
            (
                "search_messages",
                json!({ "query": "x", "match_mode": "approximate" }),
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
            response["result"]["structuredContent"]["hits"]
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
    fn mcp_json_tools_return_structured_content_matching_text_json() {
        let (dir, db) = fixture();
        let config = config_for_fixture(&dir);

        for (tool, arguments) in [
            ("search_messages", json!({ "query": "hello", "limit": 1 })),
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
