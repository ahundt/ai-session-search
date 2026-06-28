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
    // Errors are logged but non-fatal: a stale index is still useful.
    if let Err(err) = indexer::reindex(&config, &db, false, None) {
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
                    "description": "Search across all indexed AI coding sessions (Claude Code, Codex, Cursor, Antigravity, Pi) by keyword. Returns matching sessions ranked by relevance. Use this to find past work, conversations, or context from previous sessions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query (keywords, phrases, or code snippets)"
                            },
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "codex", "cursor", "antigravity", "pi"],
                                "description": "Filter by provider (optional)"
                            },
                            "path_prefix": {
                                "type": "string",
                                "description": "Filter by working-directory or repo-root prefix (optional, e.g. '~/src/sessiongrep')"
                            },
                            "since": {
                                "type": "string",
                                "description": "Lower time bound: EDTF/ISO/duration/natural language, e.g. '2026-01', '7d', 'yesterday' (optional)"
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper time bound, same formats as since (optional)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Max results to return (default 10)",
                                "default": 10
                            }
                        },
                        "required": ["query"]
                    }
                },
                {
                    "name": "get_session",
                    "description": "Get the full transcript and metadata for a specific session by its ID or ID prefix. Use this to retrieve the complete conversation from a past session.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix (e.g. 'claude:abc123' or just 'abc123')"
                            },
                            "max_lines": {
                                "type": "integer",
                                "description": "Max transcript lines to return (default: all). Use to limit context size.",
                            }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "list_sessions",
                    "description": "List recent AI coding sessions, optionally filtered by provider or path. Returns sessions sorted by most recently updated.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "codex", "cursor", "antigravity", "pi"],
                                "description": "Filter by provider (optional)"
                            },
                            "path_prefix": {
                                "type": "string",
                                "description": "Filter sessions by working directory prefix (optional)"
                            },
                            "since": {
                                "type": "string",
                                "description": "Lower time bound: EDTF/ISO/duration/natural language, e.g. '2026-01', '7d', 'yesterday' (optional)"
                            },
                            "until": {
                                "type": "string",
                                "description": "Upper time bound, same formats as since (optional)"
                            },
                            "limit": {
                                "type": "integer",
                                "description": "Max results (default 20)",
                                "default": 20
                            }
                        }
                    }
                },
                {
                    "name": "get_resume_command",
                    "description": "Get the CLI command needed to resume a specific session in its native tool (Claude Code, Codex, or Pi). Cursor and Antigravity resume are not currently supported.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": {
                                "type": "string",
                                "description": "Session ID or unique prefix"
                            }
                        },
                        "required": ["session_id"]
                    }
                },
                {
                    "name": "search_messages",
                    "description": "Search individual messages across all indexed AI coding sessions — the message-level counterpart to search_sessions. Match content by literal `query` OR `regex`; filter by `role`, `provider`, `tool`, date range (`since`/`until`), session id, and session working-directory/repo (`path_prefix`). Set `context_before`/`context_after` to include the surrounding turns of each match — useful for spotting patterns and flaws (e.g. a user correction right after an assistant action). Returns structured JSON; each hit carries session_id+seq so you can chain into get_message_context, get_session, or get_resume_command. Tip: to find where you were corrected, search role='user' with your own regex (e.g. 'wrong|stop|actually|revert').",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": { "type": "string", "description": "Case-insensitive literal substring matched in message content. Mutually exclusive with regex." },
                            "regex": { "type": "string", "description": "Rust regex matched against message content (linear-time). Mutually exclusive with query." },
                            "role": { "type": "string", "enum": ["user", "assistant", "tool", "slash", "compaction"], "description": "Restrict to one message role (optional)" },
                            "provider": { "type": "string", "enum": ["claude", "codex", "cursor", "antigravity", "pi"], "description": "Restrict to one provider (optional)" },
                            "tool": { "type": "string", "description": "Keep only tool messages whose tool name contains this, case-insensitive (optional)" },
                            "session": { "type": "string", "description": "Scope to one session id (substring/prefix, optional)" },
                            "path_prefix": { "type": "string", "description": "Restrict to sessions whose cwd or repo root starts with this path, e.g. '~/src/sessiongrep' (optional)" },
                            "since": { "type": "string", "description": "Lower time bound: EDTF/ISO/duration/natural language, e.g. '2026-01', '7d', 'yesterday' (optional)" },
                            "until": { "type": "string", "description": "Upper time bound, same formats as since (optional)" },
                            "no_compaction": { "type": "boolean", "description": "Exclude context-compaction messages (default false)" },
                            "context_before": { "type": "integer", "description": "Turns of context before each match, 0-50 (default 0)" },
                            "context_after": { "type": "integer", "description": "Turns of context after each match, 0-50 (default 0)" },
                            "limit": { "type": "integer", "description": "Max hits to return, 1-200 (default 20)", "default": 20 },
                            "offset": { "type": "integer", "description": "Skip this many hits for pagination (default 0)" },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "concise (default) truncates content to a snippet; detailed returns full content" }
                        }
                    }
                },
                {
                    "name": "get_message_context",
                    "description": "Fetch the messages surrounding a specific (session_id, seq) anchor — `before` turns before and `after` turns after — to read the conversation around a hit returned by search_messages. The anchor row is flagged with is_match=true.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "session_id": { "type": "string", "description": "Session ID (exact, as returned by search_messages)" },
                            "seq": { "type": "integer", "description": "The message seq to center on" },
                            "before": { "type": "integer", "description": "Turns before the anchor, 0-50 (default 5)" },
                            "after": { "type": "integer", "description": "Turns after the anchor, 0-50 (default 5)" },
                            "response_format": { "type": "string", "enum": ["concise", "detailed"], "description": "concise (default) truncates content; detailed returns full content" }
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
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .map(|p| p.parse::<Provider>())
        .transpose()
        .map_err(|e| e.to_string())?;

    let now = chrono::Utc::now();
    let filters = SearchFilters {
        provider,
        path_prefix: args
            .get("path_prefix")
            .and_then(Value::as_str)
            .map(normalize_path_prefix),
        since: parse_date_arg(args, "since", Bound::Start, now)?,
        until: parse_date_arg(args, "until", Bound::End, now)?,
        limit,
        warnings_only: false,
    };
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
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .map(|p| p.parse::<Provider>())
        .transpose()
        .map_err(|e| e.to_string())?;
    let path_prefix = args
        .get("path_prefix")
        .and_then(Value::as_str)
        .map(String::from);

    let now = chrono::Utc::now();
    let filters = SearchFilters {
        provider,
        path_prefix,
        since: parse_date_arg(args, "since", Bound::Start, now)?,
        until: parse_date_arg(args, "until", Bound::End, now)?,
        limit,
        warnings_only: false,
    };
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
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 200) as usize;
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let before = args
        .get("context_before")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, 50);
    let after = args
        .get("context_after")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, 50);
    let detailed = args.get("response_format").and_then(Value::as_str) == Some("detailed");

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
        since: parse_date_arg(args, "since", Bound::Start, now)?,
        until: parse_date_arg(args, "until", Bound::End, now)?,
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
    let before = args
        .get("before")
        .and_then(Value::as_i64)
        .unwrap_or(5)
        .clamp(0, 50);
    let after = args
        .get("after")
        .and_then(Value::as_i64)
        .unwrap_or(5)
        .clamp(0, 50);
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
}
