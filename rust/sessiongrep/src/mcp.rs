use std::env;
use std::io::{self, BufRead, Write};

use clap::Parser;
use serde_json::{json, Value};

use sessiongrep::config::Config;
use sessiongrep::dates::{self, Bound};
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{MessageFilters, Provider, Role, SearchFilters};
use sessiongrep::refs::{extract_refs_from_text, ref_summary};
use sessiongrep::sql_query::{
    self, DbQueryArgs, DbSchemaArgs, DEFAULT_LIMIT as DEFAULT_SQL_LIMIT,
    DEFAULT_MCP_MAX_CELL_CHARS, DEFAULT_TIMEOUT_MS as DEFAULT_SQL_TIMEOUT_MS,
};
use sessiongrep::util::{current_repo, normalize_path_prefix, resume_plan, truncate_for_display};

const DEFAULT_GET_SESSION_MAX_LINES: i64 = -40;
const TOOL_SCHEMA_SUMMARY_TABLES: usize = 4;
const TOOL_SCHEMA_SUMMARY_COLUMNS: usize = 12;
const DEFAULT_MESSAGE_SEARCH_LIMIT: usize = 20;

#[derive(Debug, Parser)]
#[command(
    name = "sessiongrep-mcp",
    version,
    about = "MCP server for sessiongrep; run without a subcommand for stdio JSON-RPC"
)]
struct McpCli {
    #[command(subcommand)]
    command: Option<sessiongrep::mcp_install::McpCmd>,
}

fn main() {
    if env::args_os().len() > 1 {
        let cli = McpCli::parse();
        if let Some(cmd) = cli.command {
            if let Err(err) = sessiongrep::mcp_install::run_mcp_cmd(cmd) {
                eprintln!("sessiongrep-mcp: {err:#}");
                std::process::exit(1);
            }
            return;
        }
    }

    let config = Config::load().expect("failed to load config");
    // Size the global thread pool for data-parallel scans from config/env/host (auto by default).
    // Non-fatal; log to STDERR only — stdout carries the JSON-RPC protocol and must stay clean.
    if let Err(err) = sessiongrep::config::init_thread_pool(config.resolve_threads()) {
        eprintln!("sessiongrep-mcp: using default thread pool ({err})");
    }
    let mut db = Db::open_with_busy_timeout(&config.db_path(), config.index.busy_timeout_ms)
        .expect("failed to open database");
    db.apply_performance_config(&config.performance);

    // Eagerly bring the index up to date on startup so the first tool call
    // doesn't pay for whatever the user has appended since the last CLI run.
    // A schema upgrade needs a full backfill first; after that the normal
    // incremental scan is enough. Errors are logged but non-fatal: a stale
    // index is still useful.
    let startup = indexer::ensure_schema_backfilled(&config, &db, None).and_then(|backfilled| {
        if backfilled {
            Ok(indexer::AutoReindexOutcome::Updated {
                files_seen: 0,
                sessions_updated: 0,
            })
        } else {
            indexer::auto_reindex(&config, &db, None)
        }
    });
    if let Err(err) = startup {
        eprintln!("sessiongrep-mcp: startup reindex failed: {err:#}");
    }

    let stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    for line in stdin.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        let response = match method {
            "initialize" => handle_initialize(id.clone()),
            "tools/list" => handle_tools_list(id.clone(), &config),
            "tools/call" => {
                maybe_reindex(&config, &db);
                handle_tools_call(id.clone(), &params, &config, &db)
            }
            "notifications/initialized" | "notifications/cancelled" => continue,
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("unknown method: {method}") }
            }),
        };

        let out = serde_json::to_string(&response).expect("failed to serialize response");
        let _ = writeln!(stdout, "{out}");
        let _ = stdout.flush();
    }
}

/// Run the shared cross-process automatic refresh if it is due. Failures are logged to stderr and
/// swallowed so a transient filesystem issue can't take the MCP server down or break a tool call
/// that can be served from the existing index.
fn maybe_reindex(config: &Config, db: &Db) {
    let outcome = indexer::auto_reindex(config, db, None);
    match outcome {
        Ok(indexer::AutoReindexOutcome::Updated { .. })
        | Ok(indexer::AutoReindexOutcome::SkippedFresh) => {}
        Ok(indexer::AutoReindexOutcome::SkippedBusy) => {
            eprintln!(
                "sessiongrep-mcp: auto-reindex skipped because another process is writing; serving existing index"
            );
        }
        Err(err) => eprintln!("sessiongrep-mcp: reindex failed: {err:#}"),
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
                "name": "sessiongrep",
                // Single source of truth: the package version, never a hand-kept duplicate.
                "version": env!("CARGO_PKG_VERSION")
            }
        }
    })
}

fn handle_tools_list(id: Option<Value>, config: &Config) -> Value {
    let schema_summary = sql_query::schema_summary_path(
        &config.db_path(),
        config.index.busy_timeout_ms,
        TOOL_SCHEMA_SUMMARY_TABLES,
        TOOL_SCHEMA_SUMMARY_COLUMNS,
    )
    .unwrap_or_else(|_| {
        "Schema unavailable until the sessiongrep index database exists; call query_session_index with no sql after indexing to inspect live AI session-history schema objects.".to_string()
    });
    let query_session_index_description = format!(
        "Inspect or query the local AI coding-agent session-history SQLite index: sessions, messages, file edits, and derived search metadata. Bounded live schema summary: {schema_summary}. For full schema, call with no sql to list session-history schema objects, or schema_table to list columns for one table/view. For content or regex search, prefer search_messages because it uses sessiongrep's FTS/trigram planner and context workflow. With sql, runs one raw read-only row-returning statement over this session-history index; it is not rewritten through the message-search planner. Opened read-only with SQLite query_only and an authorizer; writes, ATTACH/DETACH, unsafe PRAGMAs, and multiple statements are rejected."
    );
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "search_sessions",
                    "description": "Search your past AI coding-agent sessions (Claude Code, Claude Desktop local agent, Codex, Cursor, Antigravity, Pi) by keyword, ranked by relevance. Read a result with get_session, reopen it with get_resume_command, or drill into turns with search_messages.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Keywords, a phrase, or a code snippet to find in session titles and content."
                            },
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi"],
                                "description": "Only sessions from this agent. Omit for all agents."
                            },
                            "path_prefix": {
                                "type": "string",
                                "description": "Only sessions whose working directory or git repo starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory."
                            },
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
                                "type": "integer",
                                "description": "Maximum sessions to return (default 10).",
                                "default": 10
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_session",
                    "description": "Return one AI coding-agent session by session ID or unique ID prefix. By default it returns the last 40 transcript lines; set max_lines=0 only when the entire transcript is needed. Pass seq and optional context to return a focused message window around one conversation turn from search_messages.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix, e.g. 'claude:abc123' or 'abc123'."
                            },
                            "max_lines": {
                                "type": "integer",
                                "description": "Transcript lines to return in full-transcript mode: positive=head, negative=tail, 0=entire transcript and may be very large (default -40, i.e. last 40 lines). Ignored when seq is provided.",
                                "default": -40
                            },
                            "seq": {
                                "type": "integer",
                                "description": "Optional message sequence number copied from a search_messages hit. There is no default seq; provide session_id + seq to read a focused message window instead of transcript lines."
                            },
                            "context": {
                                "type": "integer",
                                "description": "When seq is provided, include this many turns before and after that message (default 0).",
                                "default": 0
                            },
                            "include_refs": {
                                "type": "boolean",
                                "description": "When seq is provided, include extracted URL/resource references for each returned message (default false).",
                                "default": false
                            },
                            "response_format": {
                                "type": "string",
                                "enum": ["concise", "detailed"],
                                "description": "When seq is provided, 'concise' (default) trims each message to a snippet; 'detailed' returns full text.",
                                "default": "concise"
                            }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "list_sessions",
                    "description": "List your most recent AI coding-agent sessions, newest first, with optional filters. To search by keyword use search_sessions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi"],
                                "description": "Only sessions from this agent. Omit for all agents."
                            },
                            "path_prefix": {
                                "type": "string",
                                "description": "Only sessions whose working directory or git repo starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory."
                            },
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
                                "type": "integer",
                                "description": "Maximum sessions to return (default 20).",
                                "default": 20
                            }
                        }
                    }
                },
                {
                    "name": "get_resume_command",
                    "description": "Return the shell command that reopens an AI coding-agent session in its original tool (Claude Code, Codex, or Pi). Claude Desktop local agent, Cursor, and Antigravity cannot be resumed.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix, e.g. 'claude:abc123' or 'abc123'."
                            }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "search_messages",
                    "description": "Search individual messages (conversation turns) across all your AI coding-agent sessions. For one-step results, set context to include turns around each match. For a larger follow-up window, call get_session with the returned session_id, seq, and context. Use full-transcript get_session only when a focused window is not enough. To find URLs, use a Rust regex such as 'https?://|www\\.|[[:alnum:].-]+\\.[[:alpha:]]{2,}' and set include_refs=true. To find where you corrected the assistant, set role=user with a regex like 'wrong|stop|actually'.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Literal text to find in message content (case-insensitive). Provide query OR regex, not both." },
                            "regex": { "type": "string", "description": "Regular expression (Rust syntax) to match message content. Provide query OR regex, not both. Regex search uses sessiongrep's trigram prefilter when selective, then verifies matches with Rust regex." },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"], "description": "Only this message role: user, assistant, tool (a tool's output), slash (a slash-command), or compaction (an auto-generated summary). Omit for all roles." },
                            "provider": { "type": "string", "enum": ["claude", "claude-desktop", "codex", "cursor", "antigravity", "pi"], "description": "Only messages from this agent. Omit for all agents." },
                            "tool": { "type": "string", "description": "Only tool messages whose tool name contains this text (case-insensitive), e.g. 'edit', 'bash'. Omit for any tool." },
                            "session": { "type": "string", "description": "Only messages from sessions whose ID contains this text. Omit for all sessions." },
                            "session_id": { "type": "string", "description": "Exact session ID or unique prefix. Prefer this when chaining from search_messages/get_session results; unlike session, it does not do substring matching." },
                            "path_prefix": { "type": "string", "description": "Only messages from sessions whose working directory or git repo starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory." },
                            "seq_from": { "type": "integer", "description": "Lower inclusive message sequence bound. Requires session_id or session because seq values are session-local." },
                            "seq_to": { "type": "integer", "description": "Upper inclusive message sequence bound. Requires session_id or session because seq values are session-local." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. A date, duration, or relative time, e.g. '2026-01-15', '202X' (whole decade), '7d' (last 7 days), 'yesterday'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound: messages at or before this. Same formats as 'since'. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single time span used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. Do not combine with since/until." },
                            "no_compaction": { "type": "boolean", "description": "Exclude auto-generated summary messages (default false).", "default": false },
                            "context": { "type": "integer", "description": "Return this many turns before and after each match in the same call (default 0). Use this for immediate one-step context.", "default": 0 },
                            "include_refs": { "type": "boolean", "description": "Include extracted URL/resource references for returned hits and context rows (default false). Use with context for source audits.", "default": false },
                            "explain": { "type": "boolean", "description": "Include planner diagnostics for regex selectivity: corpus rows, trigram prefilter, candidate rows, and a concise tuning hint. Default false.", "default": false },
                            "limit": { "type": "integer", "description": "Maximum matching messages to return (default 20).", "default": DEFAULT_MESSAGE_SEARCH_LIMIT },
                            "offset": { "type": "integer", "description": "Skip this many matches before returning, to page through results (default 0).", "default": 0 },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "'concise' (default) trims each message to a snippet; 'detailed' returns full text.", "default": "concise" }
                        }
                    }
                },
                {
                    "name": "query_session_index",
                    "description": query_session_index_description,
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "sql": { "type": "string", "description": "Exactly one raw read-only SQL statement returning rows from the local AI session-history index. Omit sql to list session-history schema objects. Prefer search_messages for accelerated content or regex search with context. Writes, ATTACH/DETACH, unsafe PRAGMAs, and multiple statements are rejected." },
                            "schema_table": { "type": "string", "description": "Optional table/view name for column details in the AI session-history index, such as sessions, messages, or file_edits. Use instead of sql." },
                            "include_internal": { "type": "boolean", "description": "When sql is omitted, include SQLite/FTS shadow tables and internal indexes for the session-history database (default false).", "default": false },
                            "limit": { "type": "integer", "description": "Maximum rows to return after the SQL statement runs (default 100). 0 means unlimited; prefer adding LIMIT in SQL for expensive queries.", "default": 100 },
                            "offset": { "type": "integer", "description": "Skip this many rows after the SQL statement runs (default 0). Prefer SQL LIMIT/OFFSET for expensive queries.", "default": 0 },
                            "timeout_ms": { "type": "integer", "description": "Interrupt the query after this many milliseconds (default 1000). 0 disables the timeout.", "default": 1000 },
                            "max_cell_chars": { "type": "integer", "description": "Maximum characters per string cell in the JSON response. 0 disables cell truncation. Default 1000.", "default": 1000 }
                        }
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
        "get_session" => tool_get_session(&args, db),
        "list_sessions" => tool_list_sessions(&args, db),
        "get_resume_command" => tool_get_resume_command(&args, db),
        "search_messages" => tool_search_messages(&args, db),
        "query_session_index" => tool_query_session_index(&args, config),
        _ => Err(format!("unknown tool: {tool_name}")),
    };

    match result {
        Ok(content) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": content }]
            }
        }),
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

fn tool_search_sessions(args: &Value, config: &Config, db: &Db) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: query")?;
    let now = chrono::Utc::now();
    let filters = search_filters_from_args(args, 10, now)?;
    let repo = current_repo(config);
    let hits = db
        .search(query, &filters, repo.as_deref(), &config.search.scoring)
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

fn tool_get_session(args: &Value, db: &Db) -> Result<String, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;
    if let Some(seq) = args.get("seq").and_then(Value::as_i64) {
        let context = args
            .get("context")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0);
        let detailed = args.get("response_format").and_then(Value::as_str) == Some("detailed");
        let include_refs = args
            .get("include_refs")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        return message_window_json(session_id, seq, context, detailed, include_refs, db);
    }
    let max_lines = args
        .get("max_lines")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_GET_SESSION_MAX_LINES);

    let full = db.resolve_session(session_id).map_err(|e| e.to_string())?;
    let s = &full.session;

    let (transcript, returned_lines) = if max_lines == 0 {
        (full.transcript_text.clone(), "all".to_string())
    } else if max_lines < 0 {
        let requested = max_lines.unsigned_abs() as usize;
        let lines: Vec<&str> = full.transcript_text.lines().collect();
        let start = lines.len().saturating_sub(requested);
        let selected = &lines[start..];
        let label = if start > 0 {
            format!("last {} (truncated; max_lines=0 returns the entire transcript and may be very large)", selected.len())
        } else {
            selected.len().to_string()
        };
        (selected.join("\n"), label)
    } else {
        let max_lines = max_lines as usize;
        let mut lines = full.transcript_text.lines();
        let selected: Vec<&str> = lines.by_ref().take(max_lines).collect();
        let truncated = lines.next().is_some();
        let label = if truncated {
            format!("first {max_lines} (truncated; max_lines=0 returns the entire transcript and may be very large)")
        } else {
            selected.len().to_string()
        };
        (selected.join("\n"), label)
    };

    let title = s.title.as_deref().unwrap_or("(untitled)");
    let cwd = s.cwd.as_deref().unwrap_or("-");
    let updated = s
        .updated_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());

    Ok(format!(
        "# {title}\n\n- ID: {}\n- Provider: {}\n- Provider Session ID: {}\n- CWD: {cwd}\n- Updated: {updated}\n- Messages: {}\n- Transcript lines returned: {returned_lines}\n\n## Transcript\n\n{transcript}",
        s.id,
        s.provider,
        s.provider_session_id,
        s.message_count.unwrap_or(0),
    ))
}

fn tool_list_sessions(args: &Value, db: &Db) -> Result<String, String> {
    let now = chrono::Utc::now();
    let filters = search_filters_from_args(args, 20, now)?;
    let sessions = db.list_recent(&filters).map_err(|e| e.to_string())?;

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

    let full = db.resolve_session(session_id).map_err(|e| e.to_string())?;
    let (command, cwd) = resume_plan(&full.session).map_err(|e| e.to_string())?;

    let cmd_str = command.join(" ");
    match cwd {
        Some(cwd) => {
            let quoted = shlex::try_quote(&cwd).map_err(|e| e.to_string())?;
            Ok(format!("cd {quoted} && {cmd_str}"))
        }
        None => Ok(cmd_str),
    }
}

fn tool_query_session_index(args: &Value, config: &Config) -> Result<String, String> {
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
            include_internal: args
                .get("include_internal")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            format: sessiongrep::render::OutputFormat::Json,
        };
        let result = sql_query::schema_path(
            &config.db_path(),
            config.index.busy_timeout_ms,
            &schema_args,
        )
        .map_err(format_mcp_query_error)?;
        let payload = sql_query::query_result_payload(&result, mcp_max_cell_chars(args));
        return serde_json::to_string_pretty(&payload.value).map_err(|e| e.to_string());
    }

    let query_args = DbQueryArgs {
        sql: sql.unwrap().to_string(),
        limit: args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SQL_LIMIT as u64) as usize,
        offset: args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize,
        timeout_ms: args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_SQL_TIMEOUT_MS),
        format: sessiongrep::render::OutputFormat::Json,
    };
    let result =
        sql_query::query_path(&config.db_path(), config.index.busy_timeout_ms, &query_args)
            .map_err(format_mcp_query_error)?;
    let payload = sql_query::query_result_payload(&result, mcp_max_cell_chars(args));
    serde_json::to_string_pretty(&payload.value).map_err(|e| e.to_string())
}

fn format_mcp_query_error(err: anyhow::Error) -> String {
    sql_query::format_query_error(
        err,
        "query_session_index",
        "call query_session_index with no sql to list AI session-history tables, or schema_table to inspect columns",
    )
}

fn mcp_max_cell_chars(args: &Value) -> usize {
    args.get("max_cell_chars")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MCP_MAX_CELL_CHARS as u64) as usize
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
        since,
        until,
        limit: args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(default_limit as u64) as usize,
        warnings_only: false,
    })
}

fn tool_search_messages(args: &Value, db: &Db) -> Result<String, String> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let regex = args.get("regex").and_then(Value::as_str).map(String::from);
    if !query.is_empty() && regex.is_some() {
        return Err("provide either `query` (literal) or `regex`, not both".to_string());
    }

    let now = chrono::Utc::now();
    // The agent manages its own context; use a small default page and report next_offset.
    // Floor at 1 so a page always makes progress; no artificial upper cap.
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MESSAGE_SEARCH_LIMIT as u64)
        .max(1) as usize;
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    // Neighbor counts are naturally bounded by the session length, so only clamp to non-negative.
    let context = args
        .get("context")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let before = context;
    let after = context;
    let detailed = args.get("response_format").and_then(Value::as_str) == Some("detailed");
    let include_refs = args
        .get("include_refs")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (since, until) = parse_date_bounds(args, now)?;
    let fuzzy_session = args
        .get("session")
        .and_then(Value::as_str)
        .map(String::from);
    let exact_session_arg = args.get("session_id").and_then(Value::as_str);
    if fuzzy_session.is_some() && exact_session_arg.is_some() {
        return Err("provide `session` OR `session_id`, not both".to_string());
    }
    let seq_from = args.get("seq_from").and_then(Value::as_i64);
    let seq_to = args.get("seq_to").and_then(Value::as_i64);
    if (seq_from.is_some() || seq_to.is_some())
        && fuzzy_session.is_none()
        && exact_session_arg.is_none()
    {
        return Err(
            "seq_from/seq_to require session_id or session because seq is session-local"
                .to_string(),
        );
    }
    if let (Some(from), Some(to)) = (seq_from, seq_to) {
        if from > to {
            return Err("seq_from must be <= seq_to".to_string());
        }
    }
    let exact_session_id = exact_session_arg
        .map(|id| db.resolve_session(id).map(|s| s.session.id))
        .transpose()
        .map_err(|e| e.to_string())?;
    let filters = MessageFilters {
        role: parse_opt_enum::<Role>(args, "role")?,
        provider: parse_opt_enum::<Provider>(args, "provider")?,
        session_id: exact_session_id,
        session: fuzzy_session,
        path_prefix: args
            .get("path_prefix")
            .and_then(Value::as_str)
            .map(normalize_path_prefix),
        since,
        until,
        seq_from,
        seq_to,
        regex,
        tool: args.get("tool").and_then(Value::as_str).map(String::from),
        no_compaction: args
            .get("no_compaction")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rank: false,
        // Fetch one past the page so we can report whether a next page exists, then slice.
        limit: offset + limit + 1,
    };
    let include_explain = args
        .get("explain")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let (mut hits, explain) = db
        .search_messages_with_explain(&query, &filters, include_explain)
        .map_err(|e| e.to_string())?;
    let explain = explain.map(|explain| {
        json!({
            "corpus": explain.corpus,
            "prefilter": explain.prefilter,
            "candidates": explain.candidates,
            "prefilter_skipped": explain.prefilter_skipped,
            "summary": explain.summary(filters.regex.is_some()),
        })
    });
    let has_more = hits.len() > offset + limit;
    let page: Vec<_> = hits.drain(..).skip(offset).take(limit).collect();
    let next_offset = has_more.then_some(offset + limit);

    // Enrich each hit with its session's cwd/repo/title in ONE batched lookup (no N+1).
    let mut ids: Vec<String> = page.iter().map(|h| h.session_id.clone()).collect();
    ids.sort();
    ids.dedup();
    let meta = db.session_metadata(&ids).map_err(|e| e.to_string())?;

    let trim = |s: &str| {
        if detailed {
            s.to_string()
        } else {
            truncate_for_display(s, 280)
        }
    };

    let hits_json: Vec<Value> = page
        .iter()
        .map(|h| {
            let m = meta.get(&h.session_id);
            let mut obj = json!({
                "session_id": h.session_id,
                "seq": h.seq,
                "role": h.role.as_str(),
                "provider": h.provider.as_str(),
                "ts": h.ts.map(|t| t.to_rfc3339()),
                "tool_name": h.tool_name,
                "cwd": m.and_then(|m| m.cwd.clone()),
                "repo": m.and_then(|m| m.repo_root.clone()),
                "title": m.and_then(|m| m.title.clone()),
                "content": trim(&h.content),
                "context_request": {
                    "tool": "get_session",
                    "arguments": {
                        "session_id": h.session_id,
                        "seq": h.seq,
                        "context": 5
                    }
                },
            });
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
                                "provider": c.provider.as_str(),
                                "ts": c.ts.map(|t| t.to_rfc3339()),
                                "tool_name": c.tool_name,
                                "is_match": c.seq == h.seq,
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
        "returned": hits_json.len(),
        "next_offset": next_offset,
        "search_explain": explain,
        "hits": hits_json,
    });
    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
}

fn message_window_json(
    session_id: &str,
    seq: i64,
    context: i64,
    detailed: bool,
    include_refs: bool,
    db: &Db,
) -> Result<String, String> {
    let before = context;
    let after = context;
    let rows = db
        .message_context(session_id, seq, before, after)
        .map_err(|e| e.to_string())?;
    let trim = |s: &str| {
        if detailed {
            s.to_string()
        } else {
            truncate_for_display(s, 280)
        }
    };
    let messages: Vec<Value> = rows
        .iter()
        .map(|c| {
            let mut row = json!({
                "seq": c.seq,
                "role": c.role.as_str(),
                "provider": c.provider.as_str(),
                "ts": c.ts.map(|t| t.to_rfc3339()),
                "tool_name": c.tool_name,
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
    let ids = vec![session_id.to_string()];
    let meta = db.session_metadata(&ids).map_err(|e| e.to_string())?;
    let session_meta = meta.get(session_id);

    let out = json!({
        "session_id": session_id,
        "anchor_seq": seq,
        "cwd": session_meta.and_then(|m| m.cwd.clone()),
        "repo": session_meta.and_then(|m| m.repo_root.clone()),
        "title": session_meta.and_then(|m| m.title.clone()),
        "messages": messages,
    });
    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessiongrep::models::Message;
    use sessiongrep::util::minimal_record;
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
        parsed.transcript_text = (0..405)
            .map(|i| format!("transcript line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mk = |seq: i64, role: Role, content: &str| Message {
            seq,
            role,
            ts: None,
            tool_name: None,
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

    fn config_for_fixture(dir: &tempfile::TempDir) -> Config {
        let mut config = Config::default();
        config.index.db_path = Some(dir.path().join("index.db").to_string_lossy().to_string());
        config
    }

    #[test]
    fn search_messages_enriches_with_session_metadata_and_paginates() {
        let (_dir, db) = fixture();

        // "hello" matches the two user turns; each hit is enriched with the session's
        // cwd/repo/title (the agent-facing context) and carries session_id+seq for chaining.
        let out = parse(&tool_search_messages(&json!({ "query": "hello" }), &db).unwrap());
        assert_eq!(out["returned"], 2);
        assert!(out["next_offset"].is_null());
        let hit = &out["hits"][0];
        assert_eq!(hit["session_id"], "claude:test1");
        assert_eq!(hit["cwd"], "/Users/x/proj");
        assert_eq!(hit["repo"], "/Users/x/proj");
        assert_eq!(hit["title"], "Proj");
        assert_eq!(hit["context_request"]["tool"], "get_session");
        assert_eq!(
            hit["context_request"]["arguments"]["session_id"],
            "claude:test1"
        );
        assert!(hit["context_request"]["arguments"]["seq"].is_number());

        // Page size 1: the first page reports a next_offset, the last page reports none.
        let p0 = parse(
            &tool_search_messages(&json!({ "query": "hello", "limit": 1, "offset": 0 }), &db)
                .unwrap(),
        );
        assert_eq!(p0["returned"], 1);
        assert_eq!(p0["next_offset"], 1);
        let p1 = parse(
            &tool_search_messages(&json!({ "query": "hello", "limit": 1, "offset": 1 }), &db)
                .unwrap(),
        );
        assert_eq!(p1["returned"], 1);
        assert!(p1["next_offset"].is_null());
    }

    #[test]
    fn search_messages_explain_reports_regex_planner_diagnostics() {
        let (_dir, db) = fixture();

        let out = parse(
            &tool_search_messages(
                &json!({
                    "regex": "hello",
                    "explain": true,
                    "limit": 1
                }),
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
        let (_dir, db) = fixture();

        // A path_prefix not containing the session filters it out entirely.
        let none = parse(
            &tool_search_messages(
                &json!({ "query": "hello", "path_prefix": "/Users/x/other" }),
                &db,
            )
            .unwrap(),
        );
        assert_eq!(none["returned"], 0);

        // A matching absolute path_prefix returns the session's messages. The fixture cwd does
        // not exist on disk, so this also exercises the lexical-absolute fallback path.
        let scoped = parse(
            &tool_search_messages(
                &json!({ "query": "hello", "path_prefix": "/Users/x/proj" }),
                &db,
            )
            .unwrap(),
        );
        assert_eq!(scoped["returned"], 2);

        // context is the simple one-step path: symmetric before/after turns are attached
        // in the search response, with the match row flagged.
        let ctx =
            parse(&tool_search_messages(&json!({ "query": "alpha", "context": 1 }), &db).unwrap());
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
        assert!(tool_search_messages(&json!({ "query": "a", "regex": "b" }), &db).is_err());
    }

    #[test]
    fn search_messages_supports_exact_session_id_and_seq_bounds() {
        let (_dir, db) = fixture();

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "hello",
                    "session_id": "claude:test1",
                    "seq_from": 1,
                    "seq_to": 2
                }),
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["returned"], 1);
        assert_eq!(out["hits"][0]["seq"], 2);

        assert!(
            tool_search_messages(&json!({ "query": "hello", "seq_from": 1 }), &db).is_err(),
            "seq bounds are session-local and must require a session scope"
        );
        assert!(
            tool_search_messages(
                &json!({ "query": "hello", "session": "test", "session_id": "claude:test1" }),
                &db
            )
            .is_err(),
            "fuzzy and exact session scopes should not be combined ambiguously"
        );
    }

    #[test]
    fn search_messages_include_refs_adds_structured_url_refs() {
        let (_dir, db) = fixture();

        let out = parse(
            &tool_search_messages(
                &json!({
                    "query": "beta",
                    "include_refs": true,
                    "response_format": "detailed"
                }),
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
                    "seq": 1,
                    "include_refs": true,
                    "response_format": "detailed"
                }),
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
    fn get_session_returns_focused_message_window_when_seq_is_provided() {
        let (_dir, db) = fixture();
        let anchor_only = parse(
            &tool_get_session(&json!({ "session_id": "claude:test1", "seq": 1 }), &db).unwrap(),
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
                &json!({ "session_id": "claude:test1", "seq": 1, "context": 1 }),
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["anchor_seq"], 1);
        assert_eq!(out["cwd"], "/Users/x/proj");
        assert_eq!(out["repo"], "/Users/x/proj");
        assert_eq!(out["title"], "Proj");
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "seq 0,1,2 in the window");
        assert!(msgs.iter().any(|m| m["seq"] == 1 && m["is_match"] == true));
        assert!(msgs.iter().any(|m| m["seq"] == 0 && m["is_match"] == false));
    }

    #[test]
    fn get_session_full_transcript_is_bounded_by_default() {
        let (_dir, db) = fixture();
        let out = tool_get_session(&json!({ "session_id": "claude:test1" }), &db).unwrap();
        assert!(out.contains("- Transcript lines returned: last 40 (truncated; max_lines=0 returns the entire transcript and may be very large)"));
        assert!(out.contains("transcript line 365"));
        assert!(out.contains("transcript line 404"));
        assert!(
            !out.contains("transcript line 364"),
            "bare get_session should not return the entire transcript by default"
        );

        let full = tool_get_session(
            &json!({ "session_id": "claude:test1", "max_lines": 0 }),
            &db,
        )
        .unwrap();
        assert!(full.contains("- Transcript lines returned: all"));
        assert!(full.contains("transcript line 404"));

        let tail = tool_get_session(
            &json!({ "session_id": "claude:test1", "max_lines": -3 }),
            &db,
        )
        .unwrap();
        assert!(tail.contains("- Transcript lines returned: last 3 (truncated; max_lines=0 returns the entire transcript and may be very large)"));
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
        assert_eq!(r["serverInfo"]["name"], "sessiongrep");
        assert!(r["capabilities"]["tools"].is_object());
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
        let query_session_index = tools
            .iter()
            .find(|t| t["name"] == "query_session_index")
            .expect("query_session_index advertised");
        assert!(get_session["description"]
            .as_str()
            .is_some_and(|d| d.contains("last 40 transcript lines")));
        assert!(query_session_index["description"]
            .as_str()
            .is_some_and(|d| {
                d.contains("Bounded live schema summary")
                    && d.contains("sessions(")
                    && d.contains("messages(")
                    && d.contains("prefer search_messages")
                    && d.contains("not rewritten through the message-search planner")
                    && !d.contains("messages_fts(")
            }));
        let sql_description = query_session_index["inputSchema"]["properties"]["sql"]
            ["description"]
            .as_str()
            .unwrap();
        assert!(sql_description.contains("raw read-only SQL"));
        assert!(sql_description.contains("Prefer search_messages"));
        assert!(query_session_index["inputSchema"]["properties"]["schema_table"].is_object());
        assert!(get_session["inputSchema"]["properties"]["seq"].is_object());
        assert!(
            get_session["inputSchema"]["properties"]["seq"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("no default seq"))
        );
        assert_eq!(
            get_session["inputSchema"]["properties"]["context"]["default"], 0,
            "context defaults to 0 unless explicitly requested"
        );
        assert_eq!(
            get_session["inputSchema"]["properties"]["max_lines"]["default"], -40,
            "bare get_session is bounded by default"
        );
        assert_eq!(
            search_messages["inputSchema"]["properties"]["context"]["default"], 0,
            "search hit expansion is opt-in"
        );
        assert_eq!(
            search_messages["inputSchema"]["properties"]["explain"]["default"], false,
            "planner diagnostics are opt-in"
        );
        assert!(
            search_messages["inputSchema"]["properties"]["regex"]["description"]
                .as_str()
                .is_some_and(|d| d.contains("trigram prefilter"))
        );
    }
}
