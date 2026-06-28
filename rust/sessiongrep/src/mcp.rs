use std::cell::Cell;
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use sessiongrep::config::Config;
use sessiongrep::dates::{self, Bound};
use sessiongrep::db::Db;
use sessiongrep::indexer;
use sessiongrep::models::{MessageFilters, Provider, Role, SearchFilters};
use sessiongrep::util::{current_repo, normalize_path_prefix, resume_plan, truncate_for_display};

/// Minimum gap between incremental reindexes triggered by MCP tool calls.
/// Agents often burst-call us (search → get → search again); the throttle
/// keeps that cheap while still surfacing new sessions promptly. The
/// incremental scan itself is dominated by `stat()` calls and is fast when
/// nothing has changed, so this is mostly a guard against pathological bursts.
const MIN_REINDEX_INTERVAL: Duration = Duration::from_millis(1500);

fn main() {
    let config = Config::load().expect("failed to load config");
    // Size the global thread pool for data-parallel scans from config/env/host (auto by default).
    // Non-fatal; log to STDERR only — stdout carries the JSON-RPC protocol and must stay clean.
    if let Err(err) = sessiongrep::config::init_thread_pool(config.resolve_threads()) {
        eprintln!("sessiongrep-mcp: using default thread pool ({err})");
    }
    let mut db = Db::open(&config.db_path()).expect("failed to open database");
    db.apply_performance_config(&config.performance);

    // Eagerly bring the index up to date on startup so the first tool call
    // doesn't pay for whatever the user has appended since the last CLI run.
    // A schema upgrade needs a full backfill first; after that the normal
    // incremental scan is enough. Errors are logged but non-fatal: a stale
    // index is still useful.
    let startup = indexer::ensure_schema_backfilled(&config, &db, None).and_then(|backfilled| {
        if backfilled {
            Ok((0, 0))
        } else {
            indexer::reindex(&config, &db, false, None)
        }
    });
    if let Err(err) = startup {
        eprintln!("sessiongrep-mcp: startup reindex failed: {err:#}");
    }
    let last_reindex: Cell<Instant> = Cell::new(Instant::now());

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
            "tools/list" => handle_tools_list(id.clone()),
            "tools/call" => {
                maybe_reindex(&config, &db, &last_reindex);
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

/// Run an incremental reindex unless we already did one in the last
/// `MIN_REINDEX_INTERVAL`. Failures are logged to stderr and swallowed so a
/// transient filesystem issue can't take the MCP server down or break a tool
/// call that could otherwise have been served from the existing index.
fn maybe_reindex(config: &Config, db: &Db, last_reindex: &Cell<Instant>) {
    if last_reindex.get().elapsed() < MIN_REINDEX_INTERVAL {
        return;
    }
    if let Err(err) = indexer::reindex(config, db, false, None) {
        eprintln!("sessiongrep-mcp: reindex failed: {err:#}");
    }
    last_reindex.set(Instant::now());
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

fn handle_tools_list(id: Option<Value>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": [
                {
                    "name": "search_sessions",
                    "description": "Search your past AI coding-agent sessions (Claude Code, Codex, Cursor, Antigravity, Pi) by keyword, ranked by relevance. Read a result with get_session, reopen it with get_resume_command, or drill into turns with search_messages.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Keywords, a phrase, or a code snippet to find in session titles and content."
                            },
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "codex", "cursor", "antigravity", "pi"],
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
                    "description": "Return one AI coding-agent session's full transcript and metadata, by session ID or unique ID prefix.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix, e.g. 'claude:abc123' or 'abc123'."
                            },
                            "max_lines": {
                                "type": "integer",
                                "description": "Maximum transcript lines to return (default: all). Lower it to save context."
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
                                "enum": ["claude", "codex", "cursor", "antigravity", "pi"],
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
                    "description": "Return the shell command that reopens an AI coding-agent session in its original tool (Claude Code, Codex, or Pi). Cursor and Antigravity cannot be resumed.",
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
                    "description": "Search individual messages (conversation turns) across all your AI coding-agent sessions. Filter by text, role, tool, time, or directory, and optionally include the turns around each match. Each result carries session_id and seq for use with get_message_context, get_session, or get_resume_command. To find where you corrected the assistant, set role=user with a regex like 'wrong|stop|actually'.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Literal text to find in message content (case-insensitive). Provide query OR regex, not both." },
                            "regex": { "type": "string", "description": "Regular expression (Rust syntax) to match message content. Provide query OR regex, not both." },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"], "description": "Only this message role: user, assistant, tool (a tool's output), slash (a slash-command), or compaction (an auto-generated summary). Omit for all roles." },
                            "provider": { "type": "string", "enum": ["claude", "codex", "cursor", "antigravity", "pi"], "description": "Only messages from this agent. Omit for all agents." },
                            "tool": { "type": "string", "description": "Only tool messages whose tool name contains this text (case-insensitive), e.g. 'edit', 'bash'. Omit for any tool." },
                            "session": { "type": "string", "description": "Only messages from sessions whose ID contains this text. Omit for all sessions." },
                            "path_prefix": { "type": "string", "description": "Only messages from sessions whose working directory or git repo starts with this path. Prefer an absolute path or '~/...'; a relative path resolves against the server's working directory. Omit to match any directory." },
                            "since": { "type": "string", "description": "Lower time bound: messages at or after this. A date, duration, or relative time, e.g. '2026-01-15', '202X' (whole decade), '7d' (last 7 days), 'yesterday'. Default: no lower bound." },
                            "until": { "type": "string", "description": "Upper time bound: messages at or before this. Same formats as 'since'. Default: no upper bound." },
                            "when": { "type": "string", "description": "Single time span used as both lower and upper bounds, e.g. '2026-01', '202X', '7d', or 'yesterday'. Do not combine with since/until." },
                            "no_compaction": { "type": "boolean", "description": "Exclude auto-generated summary messages (default false).", "default": false },
                            "context_before": { "type": "integer", "description": "Also return this many turns just before each match (default 0).", "default": 0 },
                            "context_after": { "type": "integer", "description": "Also return this many turns just after each match (default 0).", "default": 0 },
                            "limit": { "type": "integer", "description": "Maximum matching messages to return (default 20).", "default": 20 },
                            "offset": { "type": "integer", "description": "Skip this many matches before returning, to page through results (default 0).", "default": 0 },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "'concise' (default) trims each message to a snippet; 'detailed' returns full text.", "default": "concise" }
                        }
                    }
                },
                {
                    "name": "get_message_context",
                    "description": "Return the conversation turns around one message: 'before' turns before and 'after' turns after the anchor identified by session_id and seq. Use it to read what surrounded a search_messages hit. The anchor turn has is_match=true.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string", "description": "Exact session ID, as returned by search_messages." },
                            "seq": { "type": "integer", "description": "The target message's position in its session (the seq from a search_messages hit)." },
                            "before": { "type": "integer", "description": "Turns to include before the anchor (default 5).", "default": 5 },
                            "after": { "type": "integer", "description": "Turns to include after the anchor (default 5).", "default": 5 },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "'concise' (default) trims each message to a snippet; 'detailed' returns full text.", "default": "concise" }
                        },
                        "required": ["session_id", "seq"]
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
        "get_message_context" => tool_get_message_context(&args, db),
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
    let max_lines = args
        .get("max_lines")
        .and_then(Value::as_u64)
        .map(|v| v as usize);

    let full = db.resolve_session(session_id).map_err(|e| e.to_string())?;
    let s = &full.session;

    let transcript = match max_lines {
        Some(n) => full
            .transcript_text
            .lines()
            .take(n)
            .collect::<Vec<_>>()
            .join("\n"),
        None => full.transcript_text.clone(),
    };

    let title = s.title.as_deref().unwrap_or("(untitled)");
    let cwd = s.cwd.as_deref().unwrap_or("-");
    let updated = s
        .updated_at
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());

    Ok(format!(
        "# {title}\n\n- ID: {}\n- Provider: {}\n- Provider Session ID: {}\n- CWD: {cwd}\n- Updated: {updated}\n- Messages: {}\n\n## Transcript\n\n{transcript}",
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
    // Default page size 20; honor any explicit limit (the agent manages its own context).
    // Floor at 1 so a page always makes progress; no artificial upper cap.
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .max(1) as usize;
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    // Neighbor counts are naturally bounded by the session length, so only clamp to non-negative.
    let before = args
        .get("context_before")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let after = args
        .get("context_after")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .max(0);
    let detailed = args.get("response_format").and_then(Value::as_str) == Some("detailed");

    let (since, until) = parse_date_bounds(args, now)?;
    let filters = MessageFilters {
        role: parse_opt_enum::<Role>(args, "role")?,
        provider: parse_opt_enum::<Provider>(args, "provider")?,
        session: args
            .get("session")
            .and_then(Value::as_str)
            .map(String::from),
        path_prefix: args
            .get("path_prefix")
            .and_then(Value::as_str)
            .map(normalize_path_prefix),
        since,
        until,
        regex,
        tool: args.get("tool").and_then(Value::as_str).map(String::from),
        no_compaction: args
            .get("no_compaction")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        rank: false,
        // Fetch one past the page so we can report whether a next page exists, then slice.
        limit: offset + limit + 1,
        ..Default::default()
    };

    let mut hits = db
        .search_messages(&query, &filters)
        .map_err(|e| e.to_string())?;
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
            });
            if before > 0 || after > 0 {
                if let Ok(ctx) = db.message_context(&h.session_id, h.seq, before, after) {
                    let rows: Vec<Value> = ctx
                        .iter()
                        .map(|c| {
                            json!({
                                "seq": c.seq,
                                "role": c.role.as_str(),
                                "ts": c.ts.map(|t| t.to_rfc3339()),
                                "is_match": c.seq == h.seq,
                                "content": trim(&c.content),
                            })
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
        "hits": hits_json,
    });
    serde_json::to_string_pretty(&out).map_err(|e| e.to_string())
}

fn tool_get_message_context(args: &Value, db: &Db) -> Result<String, String> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or("missing required parameter: session_id")?;
    let seq = args
        .get("seq")
        .and_then(Value::as_i64)
        .ok_or("missing required parameter: seq")?;
    // Window is naturally bounded by the session length; only clamp to non-negative.
    let before = args
        .get("before")
        .and_then(Value::as_i64)
        .unwrap_or(5)
        .max(0);
    let after = args
        .get("after")
        .and_then(Value::as_i64)
        .unwrap_or(5)
        .max(0);
    let detailed = args.get("response_format").and_then(Value::as_str) == Some("detailed");

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
            json!({
                "seq": c.seq,
                "role": c.role.as_str(),
                "provider": c.provider.as_str(),
                "ts": c.ts.map(|t| t.to_rfc3339()),
                "tool_name": c.tool_name,
                "is_match": c.seq == seq,
                "content": trim(&c.content),
            })
        })
        .collect();

    let out = json!({
        "session_id": session_id,
        "anchor_seq": seq,
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
            mk(1, Role::Assistant, "beta world response"),
            mk(2, Role::User, "gamma hello again"),
        ];
        db.upsert_session(&parsed, 0, 0).unwrap();
        (dir, db)
    }

    fn parse(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
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

        // context_after attaches the following turn, with the match row flagged.
        let ctx = parse(
            &tool_search_messages(&json!({ "query": "alpha", "context_after": 1 }), &db).unwrap(),
        );
        let window = ctx["hits"][0]["context"].as_array().expect("context array");
        assert!(window
            .iter()
            .any(|m| m["is_match"] == true && m["seq"] == 0));
        assert!(
            window.iter().any(|m| m["seq"] == 1),
            "includes the next turn"
        );

        // Passing both `query` and `regex` is a clear error, not a silent precedence.
        assert!(tool_search_messages(&json!({ "query": "a", "regex": "b" }), &db).is_err());
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
    fn get_message_context_returns_window_with_anchor_flagged() {
        let (_dir, db) = fixture();
        let out = parse(
            &tool_get_message_context(
                &json!({ "session_id": "claude:test1", "seq": 1, "before": 1, "after": 1 }),
                &db,
            )
            .unwrap(),
        );
        assert_eq!(out["anchor_seq"], 1);
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3, "seq 0,1,2 in the window");
        assert!(msgs.iter().any(|m| m["seq"] == 1 && m["is_match"] == true));
        assert!(msgs.iter().any(|m| m["seq"] == 0 && m["is_match"] == false));
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
        let v = handle_tools_list(Some(json!(1)));
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
                "get_message_context",
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
    }
}
