use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

use sessiongrep::config::Config;
use sessiongrep::db::Db;
use sessiongrep::models::{Provider, SearchFilters};
use sessiongrep::util::{current_repo, resume_plan, truncate_for_display};

fn main() {
    let config = Config::load().expect("failed to load config");
    let db = Db::open(&config.db_path()).expect("failed to open database");

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
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        let response = match method {
            "initialize" => handle_initialize(id.clone()),
            "tools/list" => handle_tools_list(id.clone()),
            "tools/call" => handle_tools_call(id.clone(), &params, &config, &db),
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
                "version": "0.1.0"
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
                    "description": "Search across all indexed AI coding sessions (Claude Code, Codex, Cursor) by keyword. Returns matching sessions ranked by relevance. Use this to find past work, conversations, or context from previous sessions.",
                    "inputSchema": {
                        "type": "object",
                        "properties": {
                            "query": {
                                "type": "string",
                                "description": "Search query (keywords, phrases, or code snippets)"
                            },
                            "provider": {
                                "type": "string",
                                "enum": ["claude", "codex", "cursor", "antigravity"],
                                "description": "Filter by provider (optional)"
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
                                "enum": ["claude", "codex", "cursor", "antigravity"],
                                "description": "Filter by provider (optional)"
                            },
                            "path_prefix": {
                                "type": "string",
                                "description": "Filter sessions by working directory prefix (optional)"
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
                    "description": "Get the CLI command needed to resume a specific session in its native tool (Claude Code or Codex). Cursor transcript resume is not currently supported.",
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
                }
            ]
        }
    })
}

fn handle_tools_call(id: Option<Value>, params: &Value, config: &Config, db: &Db) -> Value {
    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result = match tool_name {
        "search_sessions" => tool_search_sessions(&args, config, db),
        "get_session" => tool_get_session(&args, db),
        "list_sessions" => tool_list_sessions(&args, db),
        "get_resume_command" => tool_get_resume_command(&args, db),
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
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(10) as usize;
    let provider = args
        .get("provider")
        .and_then(Value::as_str)
        .map(|p| p.parse::<Provider>())
        .transpose()
        .map_err(|e| e.to_string())?;

    let filters = SearchFilters {
        provider,
        path_prefix: None,
        since: None,
        limit,
        warnings_only: false,
    };
    let repo = current_repo(config);
    let hits = db
        .search(query, &filters, repo.as_deref())
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

    let full = db
        .resolve_session(session_id)
        .map_err(|e| e.to_string())?;
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

    let title = s
        .title
        .as_deref()
        .unwrap_or("(untitled)");
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
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(20) as usize;
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

    let filters = SearchFilters {
        provider,
        path_prefix,
        since: None,
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

    let full = db
        .resolve_session(session_id)
        .map_err(|e| e.to_string())?;
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
