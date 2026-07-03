use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use regex::Regex;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::models::{FileEdit, ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, parse_unix_seconds, preview_from_text, tool_call_message_content,
    truncate_for_display,
};

#[derive(Debug, Clone, Default)]
struct CodexMetadata {
    title: Option<String>,
    cwd: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    rollout_path: Option<String>,
    first_user_message: Option<String>,
    raw: serde_json::Map<String, Value>,
}

pub struct CodexAdapter {
    roots: Vec<PathBuf>,
    threads: HashMap<String, CodexMetadata>,
    index_titles: HashMap<String, String>,
    id_re: Regex,
}

impl CodexAdapter {
    pub fn new(roots: Vec<PathBuf>, codex_home: PathBuf) -> Self {
        let threads = load_threads(&codex_home.join("state_5.sqlite")).unwrap_or_default();
        let index_titles =
            load_index_titles(&codex_home.join("session_index.jsonl")).unwrap_or_default();
        Self {
            roots,
            threads,
            index_titles,
            id_re: Regex::new(r"([0-9a-f]{8}-[0-9a-f-]{27})\.jsonl$").expect("valid regex"),
        }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        let mut files = Vec::new();
        for root in &self.roots {
            if !root.exists() {
                continue;
            }
            let walker = WalkBuilder::new(root)
                .hidden(false)
                .ignore(false)
                .git_ignore(false)
                .git_exclude(false)
                .parents(false)
                .build();
            for entry in walker.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(metadata) = entry.metadata() {
                    let mtime_ns = metadata
                        .modified()
                        .ok()
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_nanos() as i64)
                        .unwrap_or_default();
                    files.push(SourceFile {
                        provider: Provider::Codex,
                        path: path.to_path_buf(),
                        mtime_ns,
                        size_bytes: metadata.len() as i64,
                    });
                }
            }
        }
        files
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => minimal_record(Provider::Codex, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let file = std::fs::File::open(path)?;
        self.parse_reader(std::io::BufReader::new(file), path)
    }

    /// Parse codex session lines from any reader. `parse_inner` calls this over the file; the
    /// incremental tail parser ([`crate::tail`]) calls it over an in-memory byte slice of the
    /// appended region, so the per-line logic lives in ONE place (a differential test asserts a
    /// tail parse equals a full parse). Streams line-by-line (task #241); `line_count` is tallied
    /// in this single pass. See claude::parse_reader for the equivalence/edge-case notes.
    pub fn parse_reader<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
    ) -> Result<ParsedSession> {
        let mut line_count: usize = 0;
        let mut provider_session_id = self
            .extract_id(path)
            .unwrap_or_else(|| "unknown".to_string());
        let mut cwd = None;
        let mut created_at = None;
        let mut updated_at = None;
        let mut transcript_lines = Vec::new();
        let mut messages = Vec::new();
        // call_id -> tool name, so a later function_call_output can be tagged with the tool.
        let mut tool_call_names: HashMap<String, String> = HashMap::new();
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut file_edit_seq: i64 = 0;
        let mut first_user = None;
        let mut last_user = None;
        let mut latest_goal: Option<Value> = None;

        for line in crate::util::lines_replacing_invalid_utf8(reader) {
            let line = line?;
            line_count += 1;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);
            match value.get("type").and_then(Value::as_str) {
                Some("session_meta") => {
                    if let Some(payload) = value.get("payload") {
                        if let Some(id) = payload.get("id").and_then(Value::as_str) {
                            provider_session_id = id.to_string();
                        }
                        if cwd.is_none() {
                            cwd = payload
                                .get("cwd")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                        }
                        if created_at.is_none() {
                            created_at = payload
                                .get("timestamp")
                                .and_then(Value::as_str)
                                .and_then(parse_datetime);
                        }
                    }
                }
                Some("response_item") => {
                    let Some(payload) = value.get("payload") else {
                        continue;
                    };
                    let item_type = payload.get("type").and_then(Value::as_str);
                    let role = payload.get("role").and_then(Value::as_str);
                    if item_type == Some("message") && matches!(role, Some("user" | "assistant")) {
                        let text = extract_text(payload);
                        if text.trim().is_empty() {
                            continue;
                        }
                        // Codex injects approval-mode context (the prior agent transcript /
                        // AGENTS.md) as a role:user message. Tag it `tool` so it stays out of
                        // user/correction analytics, the title, and the transcript — like
                        // claude's tool results.
                        if role == Some("user") && is_codex_injected_context(&text) {
                            messages.push(("tool".to_string(), text, timestamp, None));
                            continue;
                        }
                        if role == Some("user") {
                            if first_user.is_none() {
                                first_user = Some(text.clone());
                            }
                            last_user = Some(text.clone());
                        }
                        updated_at = timestamp.or(updated_at);
                        messages.push((
                            role.unwrap_or("message").to_string(),
                            text.clone(),
                            timestamp,
                            None,
                        ));
                        transcript_lines.push(format_transcript_line(
                            role.unwrap_or("message"),
                            timestamp,
                            &text,
                        ));
                    } else if matches!(item_type, Some("function_call") | Some("custom_tool_call"))
                    {
                        let name = payload.get("name").and_then(Value::as_str);
                        // Record call_id -> tool name so the matching *_output can be tagged.
                        if let (Some(call_id), Some(name)) =
                            (payload.get("call_id").and_then(Value::as_str), name)
                        {
                            tool_call_names.insert(call_id.to_string(), name.to_string());
                        }
                        if let Some(name) = name {
                            let args = codex_tool_call_args(payload);
                            messages.push((
                                "tool".to_string(),
                                tool_call_message_content(name, args),
                                timestamp,
                                Some(name.to_string()),
                            ));
                        }
                        // apply_patch carries the file changes inline; extract file edits.
                        if name == Some("apply_patch") {
                            if let Some(patch) = apply_patch_text(payload) {
                                collect_apply_patch_edits(
                                    &patch,
                                    timestamp,
                                    &mut file_edit_seq,
                                    &mut file_edits,
                                );
                            }
                        }
                    } else if matches!(
                        item_type,
                        Some("function_call_output") | Some("custom_tool_call_output")
                    ) {
                        // Tool output → a Role::Tool message tagged with the tool that
                        // produced it (correlated by call_id), kept out of the human
                        // transcript/title/preview.
                        let output = codex_output_text(payload.get("output"));
                        if !output.trim().is_empty() {
                            updated_at = timestamp.or(updated_at);
                            let tool_name = payload
                                .get("call_id")
                                .and_then(Value::as_str)
                                .and_then(|id| tool_call_names.get(id).cloned());
                            messages.push((
                                "tool".to_string(),
                                output.into_owned(),
                                timestamp,
                                tool_name,
                            ));
                        }
                    }
                }
                Some("event_msg") => {
                    let payload = value.get("payload").unwrap_or(&value);
                    let event_type = payload
                        .get("type")
                        .or_else(|| payload.get("event_type"))
                        .and_then(Value::as_str);
                    if event_type == Some("thread_goal_updated") {
                        latest_goal = payload
                            .get("goal")
                            .cloned()
                            .or_else(|| Some(payload.clone()));
                    }
                    if let Some((tool_name, content)) =
                        codex_event_tool_message(event_type, payload)
                    {
                        messages.push((
                            "tool".to_string(),
                            content.to_string(),
                            timestamp,
                            Some(tool_name),
                        ));
                    }
                }
                _ => {}
            }
        }

        let meta = self
            .threads
            .get(&provider_session_id)
            .cloned()
            .unwrap_or_default();
        let title = meta
            .title
            .or_else(|| self.index_titles.get(&provider_session_id).cloned())
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 100));
        let summary = meta
            .first_user_message
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 180));
        let cwd = cwd.or(meta.cwd);
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let created_at = created_at.or(meta.created_at);
        let updated_at = updated_at.or(meta.updated_at);
        let preview = last_user
            .clone()
            .or_else(|| first_user.clone())
            .or_else(|| summary.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let mut raw_metadata = json!({
            "line_count": line_count,
            "rollout_path": meta.rollout_path,
            "session_path": normalize_path(path),
        });
        if let Value::Object(obj) = &mut raw_metadata {
            if let Some(goal) = latest_goal {
                obj.insert("latest_goal".to_string(), goal);
            }
            for (key, value) in meta.raw {
                if !value.is_null() {
                    obj.insert(key, value);
                }
            }
        }
        let raw_metadata_json = Some(serde_json::to_string(&raw_metadata)?);

        let session = SessionRecord {
            id: format!("codex:{provider_session_id}"),
            provider: Provider::Codex,
            provider_session_id,
            title,
            summary,
            cwd,
            repo_root,
            created_at,
            updated_at,
            last_message_at: updated_at,
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
            parse_version: "codex-v2".to_string(),
            raw_metadata_json,
            parse_warning: None,
            discovery_source: "jsonl+sqlite".to_string(),
        };

        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages: crate::util::to_messages_with_tools(messages),
            file_edits,
        })
    }

    fn extract_id(&self, path: &Path) -> Option<String> {
        let value = path.to_string_lossy();
        self.id_re
            .captures(&value)
            .and_then(|captures| captures.get(1))
            .map(|match_| match_.as_str().to_string())
    }
}

/// The apply_patch payload text: codex records it as `custom_tool_call.input` (a patch
/// string); a `function_call` variant may wrap it in `arguments` JSON (`{"input": "..."}`)
/// or pass the raw patch. Returns None when no patch text is present.
fn apply_patch_text(payload: &Value) -> Option<String> {
    if let Some(input) = payload.get("input").and_then(Value::as_str) {
        return Some(input.to_string());
    }
    let args = payload.get("arguments").and_then(Value::as_str)?;
    if let Ok(value) = serde_json::from_str::<Value>(args) {
        if let Some(input) = value.get("input").and_then(Value::as_str) {
            return Some(input.to_string());
        }
    }
    args.contains("*** Begin Patch").then(|| args.to_string())
}

fn codex_tool_call_args(payload: &Value) -> Value {
    if let Some(input) = payload.get("input") {
        return input.clone();
    }
    if let Some(arguments) = payload.get("arguments") {
        if let Some(raw) = arguments.as_str() {
            return serde_json::from_str::<Value>(raw).unwrap_or_else(|_| json!(raw));
        }
        return arguments.clone();
    }
    Value::Null
}

fn codex_event_tool_message(event_type: Option<&str>, payload: &Value) -> Option<(String, Value)> {
    match event_type? {
        "thread_goal_updated" => {
            let goal = payload.get("goal").cloned().unwrap_or(Value::Null);
            Some((
                "thread_goal_updated".to_string(),
                json!({
                    "kind": "event_metadata",
                    "event_type": "thread_goal_updated",
                    "goal": goal,
                }),
            ))
        }
        "web_search_end" => {
            let action = payload.get("action").cloned().unwrap_or(Value::Null);
            Some((
                "web_search".to_string(),
                json!({
                    "kind": "event_metadata",
                    "event_type": "web_search_end",
                    "query": payload.get("query").cloned().unwrap_or(Value::Null),
                    "queries": payload
                        .get("queries")
                        .cloned()
                        .or_else(|| action.get("queries").cloned())
                        .unwrap_or(Value::Null),
                    "action": action,
                }),
            ))
        }
        "mcp_tool_call_end" => {
            let invocation = payload.get("invocation").cloned().unwrap_or(Value::Null);
            let server = invocation
                .get("server")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let tool = invocation
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let result_preview = payload
                .get("result")
                .map(extract_text)
                .filter(|text| !text.trim().is_empty())
                .map(|text| truncate_for_display(&text, 1000));
            Some((
                format!("mcp:{server}:{tool}"),
                json!({
                    "kind": "event_metadata",
                    "event_type": "mcp_tool_call_end",
                    "invocation": invocation,
                    "duration": payload.get("duration").cloned().unwrap_or(Value::Null),
                    "result_preview": result_preview,
                }),
            ))
        }
        _ => None,
    }
}

/// Parse an apply_patch payload into `(file_path, full_content?)` per file. `*** Add File:`
/// yields the new file content (its `+` lines), replayable like a Write; `*** Update File:`
/// and `*** Delete File:` are recorded path-only (a hunk is not a replayable Write/Edit
/// delta), so they appear in `files search`/`history`/`cross-ref` but not `files extract`.
fn parse_apply_patch(patch: &str) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut add_path: Option<String> = None;
    let mut add_lines: Vec<String> = Vec::new();
    fn flush(
        add_path: &mut Option<String>,
        add_lines: &mut Vec<String>,
        out: &mut Vec<(String, Option<String>)>,
    ) {
        if let Some(path) = add_path.take() {
            out.push((path, Some(std::mem::take(add_lines).join("\n"))));
        }
    }
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            flush(&mut add_path, &mut add_lines, &mut out);
            add_path = Some(path.trim().to_string());
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            flush(&mut add_path, &mut add_lines, &mut out);
            out.push((path.trim().to_string(), None));
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            flush(&mut add_path, &mut add_lines, &mut out);
            out.push((path.trim().to_string(), None));
        } else if line.starts_with("*** Begin Patch") || line.starts_with("*** End Patch") {
            flush(&mut add_path, &mut add_lines, &mut out);
        } else if add_path.is_some() {
            if let Some(content) = line.strip_prefix('+') {
                add_lines.push(content.to_string());
            }
        }
    }
    flush(&mut add_path, &mut add_lines, &mut out);
    out
}

/// Append a [`FileEdit`] for each file touched by an apply_patch payload.
fn collect_apply_patch_edits(
    patch: &str,
    ts: Option<DateTime<Utc>>,
    next_seq: &mut i64,
    out: &mut Vec<FileEdit>,
) {
    for (file_path, new_content) in parse_apply_patch(patch) {
        let file_name = crate::util::file_basename(&file_path);
        out.push(FileEdit {
            seq: *next_seq,
            ts,
            tool: "apply_patch".to_string(),
            file_path,
            file_name,
            new_content,
            edits: Vec::new(),
        });
        *next_seq += 1;
    }
}

/// Extract the textual output of a codex function/tool-call result. The `output` field is
/// normally a plain string (stdout plus a short metadata header); when it is structured,
/// fall back to its nested text/content via [`extract_text`].
fn codex_output_text<'a>(output: Option<&'a Value>) -> std::borrow::Cow<'a, str> {
    // The common case is a plain `output` string — borrow it (no clone). Only the rare
    // structured form pays `extract_text` (owned), and `None` borrows a static empty. The caller
    // checks emptiness on the borrow before ever materializing an owned `String`, so a
    // whitespace-only multi-MB output is never cloned.
    match output {
        Some(Value::String(s)) => std::borrow::Cow::Borrowed(s),
        Some(other) => std::borrow::Cow::Owned(extract_text(other)),
        None => std::borrow::Cow::Borrowed(""),
    }
}

/// True when a role:user codex message is injected context rather than real user
/// input: the approval-mode agent-history transcript codex asks the model to assess,
/// or an injected `AGENTS.md`. Excluded from user/correction analytics.
fn is_codex_injected_context(text: &str) -> bool {
    let head = text.trim_start();
    head.starts_with("The following is the Codex agent history")
        || head.starts_with("# AGENTS.md instructions")
        // Approval/goal-mode wrappers: codex resubmits the active-thread goal and hook
        // output as role:user behind these markers ("Continue working toward the active
        // thread goal …"). They are injected context, not prompts — keep them out of
        // user/correction analytics, the title, and the transcript.
        || head.starts_with("<goal_context")
        || head.starts_with("<codex_internal_context")
        || head.starts_with("<hook_prompt")
        // Codex resubmits the session environment (date/timezone/cwd/sandbox) as a role:user
        // message behind this marker on every turn — injected context, not a prompt. 156 such
        // rows were found in the real corpus polluting user analytics.
        || head.starts_with("<environment_context")
}

fn load_threads(path: &Path) -> Result<HashMap<String, CodexMetadata>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let conn = Connection::open(path)?;
    let columns = thread_columns(&conn)?;
    let opt = |name: &str| {
        if columns.iter().any(|column| column == name) {
            name.to_string()
        } else {
            format!("null as {name}")
        }
    };
    let opt_text = |name: &str| {
        if columns.iter().any(|column| column == name) {
            format!("cast({name} as text) as {name}")
        } else {
            format!("null as {name}")
        }
    };
    let sql = format!(
        "select id, title, cwd, created_at, updated_at, rollout_path, first_user_message, \
         {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} from threads",
        opt("created_at_ms"),
        opt("updated_at_ms"),
        opt("recency_at_ms"),
        opt_text("source"),
        opt_text("model_provider"),
        opt_text("sandbox_policy"),
        opt_text("approval_mode"),
        opt_text("tokens_used"),
        opt_text("git_sha"),
        opt_text("git_branch"),
        opt_text("git_origin_url"),
        opt_text("cli_version"),
        opt_text("agent_nickname"),
        opt_text("agent_role"),
        opt_text("memory_mode"),
        opt_text("model"),
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| {
        let created_at_ms = row.get::<_, Option<i64>>(7)?;
        let updated_at_ms = row.get::<_, Option<i64>>(8)?;
        let mut raw = serde_json::Map::new();
        raw.insert("created_at_ms".to_string(), json!(created_at_ms));
        raw.insert("updated_at_ms".to_string(), json!(updated_at_ms));
        raw.insert(
            "recency_at_ms".to_string(),
            json!(row.get::<_, Option<i64>>(9)?),
        );
        for (idx, key) in [
            "source",
            "model_provider",
            "sandbox_policy",
            "approval_mode",
            "tokens_used",
            "git_sha",
            "git_branch",
            "git_origin_url",
            "cli_version",
            "agent_nickname",
            "agent_role",
            "memory_mode",
            "model",
        ]
        .into_iter()
        .enumerate()
        {
            raw.insert(
                key.to_string(),
                json!(row.get::<_, Option<String>>(10 + idx)?),
            );
        }
        Ok((
            row.get::<_, String>(0)?,
            CodexMetadata {
                title: row.get::<_, Option<String>>(1)?,
                cwd: row.get::<_, Option<String>>(2)?,
                created_at: created_at_ms.and_then(parse_unix_millis).or_else(|| {
                    row.get::<_, Option<i64>>(3)
                        .ok()
                        .flatten()
                        .and_then(parse_unix_seconds)
                }),
                updated_at: updated_at_ms.and_then(parse_unix_millis).or_else(|| {
                    row.get::<_, Option<i64>>(4)
                        .ok()
                        .flatten()
                        .and_then(parse_unix_seconds)
                }),
                rollout_path: row.get::<_, Option<String>>(5)?,
                first_user_message: row.get::<_, Option<String>>(6)?,
                raw,
            },
        ))
    })?;

    let mut map = HashMap::new();
    for row in rows {
        let (id, meta) = row?;
        map.insert(id, meta);
    }
    Ok(map)
}

fn thread_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("pragma table_info(threads)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(columns)
}

fn parse_unix_millis(value: i64) -> Option<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp_millis(value)
}

fn load_index_titles(path: &Path) -> Result<HashMap<String, String>> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    // Lossy decode so a stray non-UTF-8 byte in the index sidecar never aborts codex indexing.
    let raw = String::from_utf8_lossy(&fs::read(path)?).into_owned();
    let mut map = HashMap::new();
    for line in raw.lines() {
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if let (Some(id), Some(title)) = (
            value.get("id").and_then(Value::as_str),
            value.get("thread_name").and_then(Value::as_str),
        ) {
            map.insert(id.to_string(), title.to_string());
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::{codex_output_text, is_codex_injected_context, CodexAdapter};
    use crate::models::{Provider, Role};
    use serde_json::json;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn codex_output_text_handles_string_and_structured() {
        // The common case: output is a plain stdout string.
        assert_eq!(
            codex_output_text(Some(&json!("hello\nworld"))),
            "hello\nworld"
        );
        // Defensive: a structured output falls back to its nested text.
        assert_eq!(
            codex_output_text(Some(&json!({"content": "nested out"}))),
            "nested out"
        );
        assert_eq!(codex_output_text(None), "");
    }

    #[test]
    fn indexes_function_call_output_as_tool_message_with_name() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        // Real codex shapes: a function_call (name + call_id) then its function_call_output
        // (call_id + string output), plus a normal user message.
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(
            &file,
            r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}
{"timestamp":"2026-06-25T07:06:23.136Z","type":"response_item","payload":{"type":"function_call","id":"fc_1","name":"exec_command","call_id":"call_1","arguments":"{\"cmd\":\"ls\"}"}}
{"timestamp":"2026-06-25T07:06:23.197Z","type":"response_item","payload":{"type":"function_call_output","call_id":"call_1","output":"Cargo.toml\nsrc"}}
{"timestamp":"2026-06-25T07:06:24.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"list the files"}]}}
"#,
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root], temp.path().join("nonexistent-home"));
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.provider, Provider::Codex);

        let tool_input = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains(r#""cmd":"ls""#))
            .expect("function_call input indexed as a Role::Tool message");
        assert_eq!(tool_input.tool_name.as_deref(), Some("exec_command"));
        assert!(tool_input.content.contains(r#""kind":"tool_call""#));
        let tool_output = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content == "Cargo.toml\nsrc")
            .expect("function_call_output indexed as a Role::Tool message");
        assert_eq!(tool_output.tool_name.as_deref(), Some("exec_command"));
        // The real user prompt is still indexed as a user message.
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.role == Role::User && m.content == "list the files"));
        // Tool output stays out of the human transcript.
        assert!(!parsed.transcript_text.contains("Cargo.toml"));
    }

    #[test]
    fn extracts_file_edits_from_apply_patch() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        // Real codex shape: apply_patch is a custom_tool_call whose `input` is the patch.
        fs::write(
            &file,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"019efd97-d602-7922-89dd-467272106505\",\"timestamp\":\"2026-06-25T07:00:00.000Z\",\"cwd\":\"/p\"}}\n{\"timestamp\":\"2026-06-25T07:00:01.000Z\",\"type\":\"response_item\",\"payload\":{\"type\":\"custom_tool_call\",\"status\":\"completed\",\"call_id\":\"call_1\",\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** Update File: /p/a.rs\\n@@\\n-old\\n+new\\n*** Add File: /p/b.rs\\n+line1\\n+line2\\n*** Delete File: /p/c.rs\\n*** End Patch\"}}\n",
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root], temp.path().join("no-home"));
        let parsed = adapter.parse(&adapter.discover()[0]);

        let names: Vec<&str> = parsed
            .file_edits
            .iter()
            .map(|e| e.file_name.as_str())
            .collect();
        assert!(names.contains(&"a.rs"), "Update File recorded: {names:?}");
        assert!(names.contains(&"b.rs"), "Add File recorded: {names:?}");
        assert!(names.contains(&"c.rs"), "Delete File recorded: {names:?}");
        // Added file carries its new content (replayable); update/delete are path-only.
        let added = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "b.rs")
            .unwrap();
        assert_eq!(added.tool, "apply_patch");
        assert_eq!(added.new_content.as_deref(), Some("line1\nline2"));
        let updated = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "a.rs")
            .unwrap();
        assert!(updated.new_content.is_none(), "Update File is path-only");
    }

    #[test]
    fn indexes_codex_event_metadata_and_thread_columns() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).unwrap();
        let codex_home = temp.path().join("codex-home");
        fs::create_dir_all(&codex_home).unwrap();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let state = codex_home.join("state_5.sqlite");
        let conn = rusqlite::Connection::open(&state).unwrap();
        conn.execute_batch(
            "create table threads (
                id text primary key, title text, cwd text, created_at integer, updated_at integer,
                rollout_path text, first_user_message text, model text, reasoning_effort text,
                git_branch text, created_at_ms integer, updated_at_ms integer, tokens_used integer
            );",
        )
        .unwrap();
        conn.execute(
            "insert into threads
             (id,title,cwd,created_at,updated_at,rollout_path,first_user_message,model,
              reasoning_effort,git_branch,created_at_ms,updated_at_ms,tokens_used)
             values (?1,'Thread title','/repo',1,2,'/rollout','first user','gpt-test',
                     'high','feat/x',1000,2000,42)",
            [session_id],
        )
        .unwrap();

        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(
            &file,
            concat!(
                r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/repo"}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"event_msg","payload":{"type":"thread_goal_updated","goal":{"objective":"ship literal search","status":"in_progress"}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"event_msg","payload":{"type":"web_search_end","query":"sessiongrep url search","action":{"queries":["sessiongrep url search"]}}}"#,
                "\n",
                r#"{"timestamp":"2026-06-25T07:00:03.000Z","type":"event_msg","payload":{"type":"mcp_tool_call_end","invocation":{"server":"sessiongrep","tool":"search_messages","arguments":{"query":"/goal"}},"duration":12,"result":"very long result"}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = CodexAdapter::new(vec![root], codex_home);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert_eq!(parsed.session.parse_version, "codex-v2");
        let raw = parsed.session.raw_metadata_json.as_deref().unwrap();
        assert!(raw.contains(r#""model":"gpt-test""#));
        assert!(raw.contains(r#""git_branch":"feat/x""#));
        assert!(raw.contains(r#""tokens_used":"42""#));
        assert!(raw.contains(r#""latest_goal":{"objective":"ship literal search""#));
        let debug_messages = parsed
            .messages
            .iter()
            .map(|message| format!("{:?}: {}", message.tool_name, message.content))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            parsed.messages.iter().any(|message| {
                message.tool_name.as_deref() == Some("thread_goal_updated")
                    && message.content.contains("ship literal search")
            }),
            "{debug_messages}"
        );
        assert!(
            parsed.messages.iter().any(|message| {
                message.tool_name.as_deref() == Some("web_search")
                    && message.content.contains("sessiongrep url search")
            }),
            "{debug_messages}"
        );
        assert!(
            parsed.messages.iter().any(|message| {
                message.tool_name.as_deref() == Some("mcp:sessiongrep:search_messages")
                    && message.content.contains(r#""query":"/goal""#)
            }),
            "{debug_messages}"
        );
    }

    #[test]
    fn detects_injected_context_not_real_prompts() {
        assert!(is_codex_injected_context(
            "The following is the Codex agent history whose request action you are assessing."
        ));
        assert!(is_codex_injected_context(
            "# AGENTS.md instructions for /Users/x/proj"
        ));
        assert!(is_codex_injected_context(
            "# AGENTS.md instructions <INSTRUCTIONS> <!-- autorun -->"
        ));
        // Approval/goal-mode injections: codex wraps the active-thread goal and hook
        // output in these markers and submits them as role:user. They are not prompts.
        assert!(is_codex_injected_context(
            "<goal_context>\nContinue working toward the active thread goal."
        ));
        assert!(is_codex_injected_context(
            "<codex_internal_context source=\"goal\">\nContinue working toward the active thread goal."
        ));
        assert!(is_codex_injected_context(
            "<hook_prompt hook_run_id=\"stop:5:/home/x/.codex/hooks.json\">usage: autorun"
        ));
        // Per-turn environment context (date/timezone/cwd/sandbox) resubmitted as role:user.
        assert!(is_codex_injected_context(
            "<environment_context>\n<current_date>2026-05-22</current_date>\n</environment_context>"
        ));
        // Leading whitespace must not defeat the marker match.
        assert!(is_codex_injected_context("\n  <goal_context>\nwork"));
        assert!(!is_codex_injected_context("please fix the failing test"));
        assert!(!is_codex_injected_context(
            "revert that change, it broke the build"
        ));
        // A real prompt that merely mentions the word goal must not be filtered.
        assert!(!is_codex_injected_context(
            "the goal context here is wrong, redo it"
        ));
    }

    /// Differential guard for the streaming-parse refactor (task #241): identical output
    /// and `line_count` between the streaming `BufReader` path and the prior whole-file
    /// `fs::read_to_string` path. Fixture exercises blank/malformed lines and a final line
    /// without a trailing newline (line_count must be 5).
    #[test]
    fn streaming_parse_output_is_stable() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        // 4 physical lines, final line has no trailing newline:
        //   1 session_meta  2 user message  3 malformed (skipped)  4 assistant (no \n)
        let content = concat!(
            r#"{"type":"session_meta","payload":{"id":"019efd97-d602-7922-89dd-467272106505","timestamp":"2026-06-25T07:00:00.000Z","cwd":"/tmp/proj"}}"#,
            "\n",
            r#"{"timestamp":"2026-06-25T07:00:01.000Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"text","text":"do the thing"}]}}"#,
            "\n",
            "{bad json line\n",
            r#"{"timestamp":"2026-06-25T07:00:02.000Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
        );
        fs::write(&file, content).unwrap();

        let adapter = CodexAdapter::new(vec![root], temp.path().join("no-home"));
        let parsed = adapter.parse(&adapter.discover()[0]);

        assert!(
            parsed
                .session
                .raw_metadata_json
                .as_deref()
                .unwrap()
                .contains("\"line_count\":4"),
            "line_count must be 4 (4 physical lines), got: {:?}",
            parsed.session.raw_metadata_json
        );
        assert_eq!(parsed.session.provider, Provider::Codex);
        assert_eq!(parsed.session.cwd.as_deref(), Some("/tmp/proj"));
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["do the thing", "done"]);
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert!(parsed.transcript_text.contains("do the thing"));
        assert!(parsed.transcript_text.contains("done"));
    }

    /// Non-UTF-8 bytes must never panic or abort the parse — they are decoded lossily (U+FFFD).
    /// This input is not valid JSON even after lossy decoding, so it yields no messages, but
    /// parsing completes WITHOUT error (lossy recovery is not treated as a parse failure).
    #[test]
    fn non_utf8_garbage_parses_gracefully_without_error() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "019efd97-d602-7922-89dd-467272106505";
        let file = root.join(format!("rollout-2026-06-25T03-04-06-{session_id}.jsonl"));
        fs::write(&file, [b'{', 0xFF, 0xFE, b'}', b'\n']).unwrap();

        let adapter = CodexAdapter::new(vec![root], temp.path().join("no-home"));
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert!(parsed.messages.is_empty());
        assert!(
            parsed.session.parse_warning.is_none(),
            "lossy recovery is not an error, so no parse warning is set"
        );
        assert_eq!(parsed.session.message_count, Some(0));
    }
}
