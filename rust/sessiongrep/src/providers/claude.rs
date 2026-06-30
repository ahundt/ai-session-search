use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{json, Value};

use crate::models::{EditOp, FileEdit, ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, preview_from_text, substantive_text, truncate_for_display,
};

pub struct ClaudeAdapter {
    roots: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeSourceKind {
    CodeJsonl,
    DesktopLocalAgent,
}

#[derive(Debug, Default)]
struct ClaudeDesktopMetadata {
    session_id: Option<String>,
    cli_session_id: Option<String>,
    cwd: Option<String>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    title: Option<String>,
    initial_message: Option<String>,
    sidecar_path: Option<PathBuf>,
}

impl ClaudeAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
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
                if path.components().any(|component| {
                    let value = component.as_os_str();
                    value == "memory" || value == "subagents"
                }) {
                    continue;
                }
                if let Ok(metadata) = entry.metadata() {
                    let mtime_ns = metadata
                        .modified()
                        .ok()
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_nanos() as i64)
                        .unwrap_or_default();
                    let source_kind = ClaudeSourceKind::from_path(path);
                    files.push(SourceFile {
                        provider: source_kind.provider(),
                        path: path.to_path_buf(),
                        mtime_ns,
                        size_bytes: metadata.len() as i64,
                    });
                    if is_claude_desktop_audit(path) {
                        if let Some(sidecar) = claude_desktop_sidecar_path(path) {
                            if let Ok(sidecar_metadata) = sidecar.metadata() {
                                if let Some(source) = files.last_mut() {
                                    let sidecar_mtime_ns = sidecar_metadata
                                        .modified()
                                        .ok()
                                        .and_then(|value| {
                                            value.duration_since(std::time::UNIX_EPOCH).ok()
                                        })
                                        .map(|value| value.as_nanos() as i64)
                                        .unwrap_or_default();
                                    source.mtime_ns = source.mtime_ns.max(sidecar_mtime_ns);
                                    source.size_bytes = source
                                        .size_bytes
                                        .saturating_add(sidecar_metadata.len() as i64);
                                }
                            }
                        }
                    }
                }
            }
        }
        files
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => minimal_record(
                ClaudeSourceKind::from_path(&source.path).provider(),
                &source.path,
                err.to_string(),
            ),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let file = std::fs::File::open(path)?;
        self.parse_reader(std::io::BufReader::new(file), path)
    }

    /// Parse claude session lines from any reader. `parse_inner` calls this over the file; the
    /// incremental tail parser ([`crate::tail`]) calls it over an in-memory byte slice of the
    /// appended region. Keeping the per-line logic in ONE place (no tail-specific copy) is what
    /// lets a differential test assert a tail parse equals a full parse.
    ///
    /// Streams line-by-line via the reader instead of loading the whole file into a String
    /// (task #241): a 536MB append-only session previously needed ~1.5GB transient RAM for the
    /// `read_to_string` String plus a second `lines().count()` pass. We hold only the current
    /// line and tally `line_count` in this single pass. `BufRead::lines()` yields the same line
    /// content/count as `str::lines()` (verified for `\n`, `\r\n`, trailing/no-trailing newline)
    /// and reads each line via [`crate::util::lines_replacing_invalid_utf8`]: a stray non-UTF-8
    /// byte becomes U+FFFD rather than aborting the parse, so one bad byte never loses the session.
    pub fn parse_reader<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
    ) -> Result<ParsedSession> {
        let source_kind = ClaudeSourceKind::from_path(path);
        let desktop = match source_kind {
            ClaudeSourceKind::CodeJsonl => ClaudeDesktopMetadata::default(),
            ClaudeSourceKind::DesktopLocalAgent => claude_desktop_metadata(path),
        };
        let mut line_count: usize = 0;
        let mut malformed_line_count: usize = 0;
        let mut provider_session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        if let Some(session_id) = desktop.session_id.as_deref() {
            provider_session_id = session_id.to_string();
        } else if source_kind == ClaudeSourceKind::DesktopLocalAgent {
            if let Some(session_id) = claude_desktop_session_id_from_path(path) {
                provider_session_id = session_id;
            }
        }
        let mut cwd = desktop.cwd.clone();
        let mut created_at: Option<DateTime<Utc>> = desktop.created_at;
        let mut updated_at: Option<DateTime<Utc>> = desktop.updated_at;
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();
        let mut last_prompt = desktop.initial_message.clone();
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut file_edit_seq: i64 = 0;
        // tool_use_id -> tool name, so a later tool_result (which references the call by
        // id, not name) can be tagged with the tool it came from.
        let mut tool_use_names: HashMap<String, String> = HashMap::new();

        for line in crate::util::lines_replacing_invalid_utf8(reader) {
            let line = line?;
            line_count += 1;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => {
                    malformed_line_count += 1;
                    continue;
                }
            };
            if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                provider_session_id = session_id.to_string();
            }
            if let Some(session_id) = value.get("session_id").and_then(Value::as_str) {
                provider_session_id = session_id.to_string();
            }
            if value.get("type").and_then(Value::as_str) == Some("last-prompt") {
                if let Some(prompt) = value.get("lastPrompt").and_then(Value::as_str) {
                    let prompt = prompt.trim();
                    if substantive_text(prompt) {
                        last_prompt = Some(prompt.to_string());
                    }
                }
            }
            if cwd.is_none() {
                cwd = value
                    .get("cwd")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
            }

            let timestamp = claude_timestamp(&value, source_kind);

            let mut role = value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut text = String::new();
            let mut tool_result = false;
            let mut tool_name: Option<String> = None;

            if let Some(message) = value.get("message") {
                role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(role);
                text = extract_text(message);
                // Capture file-mutating tool calls before any text-based skip/continue,
                // so edits inside assistant turns with empty/skipped text are still recorded.
                collect_file_edits(message, timestamp, &mut file_edit_seq, &mut file_edits);
                collect_tool_use_names(message, &mut tool_use_names);
                // One scan for the first tool_result block: `tool_result` is true when a block
                // EXISTS (even without a `tool_use_id`), and the name is tagged from that same
                // block's id when present. Was two scans (`is_tool_result` + `tool_result_id`).
                if let Some(block) = first_tool_result_block(message) {
                    tool_result = true;
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        tool_name = tool_use_names.get(id).cloned();
                    }
                }
            } else if let Some(message) = value.get("content").and_then(Value::as_str) {
                text = message.to_string();
            }

            if should_skip_message(&value, &text) {
                continue;
            }
            // Compaction summaries are `/compact` output that Claude records as role:user
            // with `isCompactSummary: true` — a continuation digest, not a real prompt.
            let is_compaction = value
                .get("isCompactSummary")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let text = strip_command_markup(&text);

            match role.as_deref() {
                Some("user") | Some("assistant") => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    // Compaction digest → `compaction` role; tool output (claude records
                    // tool results as role:user) → `tool`. Both are searchable but excluded
                    // from user/correction/planning analytics and the human transcript
                    // (kept separate from the conversation, like other providers' tool output).
                    if is_compaction {
                        messages.push(("compaction".to_string(), text, timestamp, None));
                        continue;
                    }
                    if tool_result {
                        messages.push(("tool".to_string(), text, timestamp, tool_name));
                        continue;
                    }
                    if created_at.is_none() {
                        created_at = timestamp;
                    }
                    if timestamp.is_some() {
                        updated_at = timestamp;
                    }
                    messages.push((role.unwrap_or_default(), text.clone(), timestamp, None));
                    transcript_lines.push(format_transcript_line(
                        messages
                            .last()
                            .map(|(role, _, _, _)| role.as_str())
                            .unwrap_or("message"),
                        timestamp,
                        &text,
                    ));
                }
                _ => {}
            }
        }

        let first_user = messages
            .iter()
            .find(|(role, text, _, _)| role == "user" && substantive_text(text))
            .map(|(_, text, _, _)| text.clone());
        let last_user = messages
            .iter()
            .rev()
            .find(|(role, text, _, _)| role == "user" && substantive_text(text))
            .map(|(_, text, _, _)| text.clone());
        let title = desktop
            .title
            .clone()
            .filter(|text| substantive_text(text))
            .or_else(|| {
                last_prompt
                    .clone()
                    .filter(|text| substantive_text(text))
                    .map(|text| truncate_for_display(&text, 100))
            })
            .or_else(|| {
                last_user
                    .clone()
                    .map(|text| truncate_for_display(&text, 100))
            })
            .or_else(|| {
                first_user
                    .clone()
                    .map(|text| truncate_for_display(&text, 100))
            });
        let preview = last_prompt
            .clone()
            .or_else(|| last_user.clone())
            .or_else(|| first_user.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let mut raw_metadata = json!({
            "line_count": line_count,
            "session_path": normalize_path(path),
        });
        if malformed_line_count > 0 {
            raw_metadata["malformed_line_count"] = json!(malformed_line_count);
        }
        if let Some(path) = desktop.sidecar_path.as_deref() {
            raw_metadata["metadata_path"] = json!(normalize_path(path));
        }
        if let Some(cli_session_id) = desktop.cli_session_id.as_deref() {
            raw_metadata["cli_session_id"] = json!(cli_session_id);
        }
        let raw_metadata_json = Some(serde_json::to_string(&raw_metadata)?);

        let parse_warning =
            if source_kind == ClaudeSourceKind::DesktopLocalAgent && malformed_line_count > 0 {
                Some(format!(
                    "skipped {malformed_line_count} malformed JSONL line(s)"
                ))
            } else {
                None
            };

        let provider = source_kind.provider();
        let session = SessionRecord {
            id: format!("{provider}:{provider_session_id}"),
            provider,
            provider_session_id,
            title,
            summary: first_user.map(|text| truncate_for_display(&text, 180)),
            cwd,
            repo_root,
            created_at,
            updated_at,
            last_message_at: updated_at,
            preview_text: preview,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
            parse_version: source_kind.parse_version().to_string(),
            raw_metadata_json,
            parse_warning,
            discovery_source: source_kind.discovery_source().to_string(),
        };

        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages: crate::util::to_messages_with_tools(messages),
            file_edits,
        })
    }
}

impl ClaudeSourceKind {
    fn from_path(path: &Path) -> Self {
        if is_claude_desktop_audit(path) {
            Self::DesktopLocalAgent
        } else {
            Self::CodeJsonl
        }
    }

    fn parse_version(self) -> &'static str {
        match self {
            Self::CodeJsonl => "claude-v1",
            Self::DesktopLocalAgent => "claude-desktop-local-agent-v1",
        }
    }

    fn discovery_source(self) -> &'static str {
        match self {
            Self::CodeJsonl => "jsonl",
            Self::DesktopLocalAgent => "claude-desktop-local-agent-audit-jsonl",
        }
    }

    fn provider(self) -> Provider {
        match self {
            Self::CodeJsonl => Provider::Claude,
            Self::DesktopLocalAgent => Provider::ClaudeDesktop,
        }
    }
}

fn claude_timestamp(value: &Value, source_kind: ClaudeSourceKind) -> Option<DateTime<Utc>> {
    let primary = match source_kind {
        ClaudeSourceKind::CodeJsonl => "timestamp",
        ClaudeSourceKind::DesktopLocalAgent => "_audit_timestamp",
    };
    value
        .get(primary)
        .and_then(Value::as_str)
        .and_then(parse_datetime)
        .or_else(|| {
            value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime)
        })
}

pub(crate) fn is_claude_desktop_audit(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("audit.jsonl")
        && path
            .components()
            .any(|component| component.as_os_str() == "local-agent-mode-sessions")
}

fn claude_desktop_session_id_from_path(path: &Path) -> Option<String> {
    let name = path.parent()?.file_name()?.to_str()?;
    Some(
        name.strip_prefix("local_")
            .filter(|id| !id.is_empty())
            .unwrap_or(name)
            .to_string(),
    )
}

fn claude_desktop_sidecar_path(path: &Path) -> Option<PathBuf> {
    let session_dir = path.parent()?;
    let session_dir_name = session_dir.file_name()?.to_str()?;
    Some(
        session_dir
            .parent()?
            .join(format!("{session_dir_name}.json")),
    )
}

fn claude_desktop_metadata(path: &Path) -> ClaudeDesktopMetadata {
    let Some(sidecar_path) = claude_desktop_sidecar_path(path) else {
        return ClaudeDesktopMetadata::default();
    };
    let Ok(raw) = fs::read_to_string(&sidecar_path) else {
        return ClaudeDesktopMetadata {
            sidecar_path: Some(sidecar_path),
            ..ClaudeDesktopMetadata::default()
        };
    };
    let Ok(value) = serde_json::from_str::<Value>(&raw) else {
        return ClaudeDesktopMetadata {
            sidecar_path: Some(sidecar_path),
            ..ClaudeDesktopMetadata::default()
        };
    };
    let get_str = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
    ClaudeDesktopMetadata {
        session_id: get_str("sessionId").or_else(|| {
            get_str("session_id").or_else(|| claude_desktop_session_id_from_path(path))
        }),
        cli_session_id: get_str("cliSessionId"),
        cwd: get_str("cwd"),
        created_at: value
            .get("createdAt")
            .and_then(Value::as_str)
            .and_then(parse_datetime),
        updated_at: value
            .get("lastActivityAt")
            .or_else(|| value.get("updatedAt"))
            .and_then(Value::as_str)
            .and_then(parse_datetime),
        title: get_str("title").map(|text| truncate_for_display(&text, 100)),
        initial_message: get_str("initialMessage"),
        sidecar_path: Some(sidecar_path),
    }
}

/// Scan an assistant `message.content` array for `tool_use` blocks that mutate a
/// file (`Write`/`Edit`/`MultiEdit`/`NotebookEdit`) and append a [`FileEdit`] for
/// each, assigning monotonic session-local sequence numbers.
pub(crate) fn collect_file_edits(
    message: &Value,
    ts: Option<DateTime<Utc>>,
    next_seq: &mut i64,
    out: &mut Vec<FileEdit>,
) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let input = block.get("input");
        if let Some((file_path, new_content, edits)) = tool_use_payload(name, input) {
            let file_name = crate::util::file_basename(&file_path);
            out.push(FileEdit {
                seq: *next_seq,
                ts,
                tool: name.to_string(),
                file_path,
                file_name,
                new_content,
                edits,
            });
            *next_seq += 1;
        }
    }
}

/// `(file_path, full_content?, edit deltas)` for one file-mutating tool call.
type ToolEditPayload = (String, Option<String>, Vec<EditOp>);

/// Map a single file-mutating tool call to `(file_path, full_content?, edits)`.
/// `Write` yields a full-content snapshot; `Edit`/`MultiEdit` yield delta ops (carrying
/// the `replace_all` flag); `NotebookEdit` is recorded (path only) so it appears in
/// history/cross-ref, but carries no replayable delta (cell reconstruction is out of scope).
fn tool_use_payload(name: &str, input: Option<&Value>) -> Option<ToolEditPayload> {
    let input = input?;
    let str_field = |key: &str| input.get(key).and_then(Value::as_str).map(str::to_string);
    match name {
        "Write" => {
            let file_path = str_field("file_path")?;
            let content = str_field("content").unwrap_or_default();
            Some((file_path, Some(content), Vec::new()))
        }
        "Edit" => {
            let file_path = str_field("file_path")?;
            let old = str_field("old_string").unwrap_or_default();
            let new = str_field("new_string").unwrap_or_default();
            let replace_all = input
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some((
                file_path,
                None,
                vec![EditOp {
                    old,
                    new,
                    replace_all,
                }],
            ))
        }
        "MultiEdit" => {
            let file_path = str_field("file_path")?;
            let edits = input
                .get("edits")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| {
                            let old = item.get("old_string").and_then(Value::as_str)?;
                            let new = item.get("new_string").and_then(Value::as_str)?;
                            let replace_all = item
                                .get("replace_all")
                                .and_then(Value::as_bool)
                                .unwrap_or(false);
                            Some(EditOp {
                                old: old.to_string(),
                                new: new.to_string(),
                                replace_all,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some((file_path, None, edits))
        }
        "NotebookEdit" => {
            let file_path = str_field("notebook_path").or_else(|| str_field("file_path"))?;
            Some((file_path, None, Vec::new()))
        }
        // Cursor's primary edit tool: a unified-diff patch. Recorded path-only (the diff is
        // not a replayable Write/Edit delta), so it shows up in files search/history/cross-ref
        // but is not reconstructable via `files extract`.
        "ApplyPatch" => {
            let file_path = str_field("path").or_else(|| str_field("file_path"))?;
            Some((file_path, None, Vec::new()))
        }
        _ => None,
    }
}

fn tag_content<'a>(text: &'a str, tag: &str) -> &'a str {
    let open = &format!("<{tag}>");
    let close = &format!("</{tag}>");
    text.find(open.as_str())
        .map(|i| &text[i + open.len()..])
        .and_then(|s| s.find(close.as_str()).map(|j| &s[..j]))
        .unwrap_or("")
}

/// For slash-command invocations, return `"<command-name> <command-args>"` so the
/// command identity survives the markup strip. Keeping the leading `/name` is what
/// lets `classify_role` mark the turn `Role::Slash` and lets planning aggregation
/// recover the command via `slash_command_token` — dropping it (as the previous
/// args-only strip did) misclassified every real slash command as a plain user
/// message and undercounted planning usage. Messages without the markup pass through
/// unchanged; no-arg invocations are dropped earlier by `should_skip_message`.
fn strip_command_markup(text: &str) -> String {
    if !text.contains("<command-name>") {
        return text.to_string();
    }
    let mut name = tag_content(text, "command-name").trim().to_string();
    if !name.is_empty() && !name.starts_with('/') {
        name.insert(0, '/');
    }
    let args = tag_content(text, "command-args").trim();
    match (name.is_empty(), args.is_empty()) {
        (true, _) => args.to_string(),
        (false, true) => name,
        (false, false) => format!("{name} {args}"),
    }
}

/// The first `tool_result` content block of a (role:user) message, if any. Single scan shared by
/// [`is_tool_result`] (block EXISTS → classify as `tool`) and [`tool_result_id`] (the block's
/// `tool_use_id`, which may be absent even when the block exists).
pub(crate) fn first_tool_result_block(message: &Value) -> Option<&Value> {
    message
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .find(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
}

/// True when a (role:user) message is actually a tool result — its `content` array
/// carries a `tool_result` block. Claude Code records tool output this way, so these
/// must be classified `tool`, not `user`, to keep user/correction analytics clean. NOTE: a
/// `tool_result` block may carry no `tool_use_id`; it is STILL a tool result (only the name
/// tag is unavailable), so this must not collapse to `tool_result_id(...).is_some()`.
pub(crate) fn is_tool_result(message: &Value) -> bool {
    first_tool_result_block(message).is_some()
}

/// Record `tool_use_id -> tool name` for every `tool_use` block in an assistant message.
/// A later `tool_result` references its call by id but does not repeat the tool name, so
/// this map lets the tool-output message be tagged with the tool it came from.
pub(crate) fn collect_tool_use_names(message: &Value, out: &mut HashMap<String, String>) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        if let (Some(id), Some(name)) = (
            block.get("id").and_then(Value::as_str),
            block.get("name").and_then(Value::as_str),
        ) {
            out.insert(id.to_string(), name.to_string());
        }
    }
}

/// The `tool_use_id` of the first `tool_result` block in a (role:user) message — the key
/// to look up the originating tool's name in the [`collect_tool_use_names`] map.
pub(crate) fn tool_result_id(message: &Value) -> Option<&str> {
    first_tool_result_block(message)
        .and_then(|block| block.get("tool_use_id").and_then(Value::as_str))
}

fn should_skip_message(value: &Value, text: &str) -> bool {
    let normalized = text.trim();
    // `isMeta` is Claude Code's marker for bookkeeping injected as role:user — local
    // command caveats, hook feedback ("Stop hook feedback: …"), system notices. None are
    // real conversation, so they are dropped from the index entirely (not just caveats).
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    // Background-task completion notices are injected as role:user (userType "external")
    // WITHOUT an isMeta flag, so the guard above misses them. They are harness bookkeeping
    // (a subagent finished), not conversation — drop them like other injected meta.
    if normalized.starts_with("<task-notification") {
        return true;
    }
    // Local-command machinery recorded as role:user — slash-command stdout/stderr and caveats
    // (e.g. `/model` "Set model to …" stdout, `/compact` PreCompact hook output). These are
    // harness output, not user prompts; without this skip they pollute user analytics
    // (corrections, similar-message search, user search) — 647 such rows were found in the real corpus. The
    // empty type:"system" stdout variant is already ignored (non user/assistant role); this
    // catches the type:"user" content-string form that leaks through.
    if normalized.starts_with("<local-command-stdout>")
        || normalized.starts_with("<local-command-stderr>")
        || normalized.starts_with("<local-command-caveat>")
    {
        return true;
    }
    // Skip slash command invocations that carry no args — pure UI bookkeeping.
    // Invocations with args (e.g. `/brutal-review <url>`) pass through; strip_command_markup
    // then reduces them to just the args text.
    (normalized.contains("<command-name>")
        && tag_content(normalized, "command-args").trim().is_empty())
        || normalized.eq_ignore_ascii_case("resume cancelled")
}

#[cfg(test)]
mod tests {
    use super::should_skip_message;
    use crate::models::Provider;
    use serde_json::json;

    #[test]
    fn skips_local_command_output_recorded_as_user() {
        // `/model`, `/compact`-hook etc. record their stdout/stderr as a role:user message
        // (type:"user", content is a bare string). It is harness output, not a prompt, and must
        // be skipped so it never pollutes user analytics (647 such rows existed in the corpus).
        let value = json!({ "type": "user", "message": {"role": "user"} });
        assert!(should_skip_message(
            &value,
            "<local-command-stdout>Set model to Opus 4.8 and saved as your default</local-command-stdout>"
        ));
        assert!(should_skip_message(
            &value,
            "<local-command-stderr>boom</local-command-stderr>"
        ));
        assert!(should_skip_message(
            &value,
            "<local-command-caveat>note</local-command-caveat>"
        ));
        // A real prompt that merely mentions the tag name (not leading with it) is kept.
        assert!(!should_skip_message(
            &value,
            "what does <local-command-stdout> mean when it shows up in the logs"
        ));
    }

    #[test]
    fn skips_local_command_caveat_meta_messages() {
        let value = json!({
            "isMeta": true,
            "message": {
                "role": "user",
                "content": "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"
            }
        });
        let text = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>";
        assert!(should_skip_message(&value, text));
    }

    #[test]
    fn keeps_normal_user_messages() {
        let value = json!({
            "isMeta": false,
            "message": {
                "role": "user",
                "content": "real prompt"
            }
        });
        assert!(!should_skip_message(&value, "real prompt"));
    }

    #[test]
    fn skips_meta_hook_feedback() {
        // Hook output is injected as a meta role:user message with arbitrary text.
        let text = "Stop hook feedback: 🛑 CANNOT STOP — incomplete tasks: 1. #24";
        let value = json!({ "isMeta": true, "message": {"role": "user", "content": text} });
        assert!(should_skip_message(&value, text));
    }

    #[test]
    fn skips_background_task_notifications() {
        // Background-task completion notices are injected as role:user with userType
        // "external" and NO isMeta flag, so they slip past the isMeta guard. They are
        // harness bookkeeping (a subagent finished), not user conversation.
        let text = "<task-notification>\n<task-id>bbawn9c36</task-id>\n<tool-use-id>toolu_01</tool-use-id>\n<output-file>/tmp/out.txt</output-file>\nAgent completed.";
        let value = json!({ "isMeta": false, "message": {"role": "user", "content": text} });
        assert!(should_skip_message(&value, text));
        // Leading whitespace must not defeat the match.
        let padded = format!("\n  {text}");
        assert!(should_skip_message(&value, &padded));
        // A real prompt that merely mentions the word must not be skipped.
        assert!(!should_skip_message(
            &value,
            "the task-notification format is confusing, can you explain it"
        ));
    }

    #[test]
    fn skips_no_arg_slash_commands() {
        for cmd in &[
            "/exit", "/resume", "/clear", "/compact", "/mcp", "/config", "/help",
        ] {
            let text = format!("<command-name>{cmd}</command-name><command-message>{cmd}</command-message><command-args></command-args>");
            let value = json!({ "isMeta": false });
            assert!(
                should_skip_message(&value, &text),
                "should skip {cmd} (no args)"
            );
        }
    }

    #[test]
    fn keeps_slash_commands_with_args() {
        let text = "<command-name>/brutal-review</command-name><command-message>brutal-review</command-message><command-args>https://github.com/braincompany/sessiongrep/pull/15</command-args>";
        let value = json!({ "isMeta": false });
        assert!(!should_skip_message(&value, text));
    }

    #[test]
    fn strip_command_markup_preserves_command_name_and_args() {
        // The command name is kept so the turn classifies as Role::Slash and planning
        // aggregation can recover `/brutal-review` via slash_command_token.
        let text = "<command-name>/brutal-review</command-name><command-message>brutal-review</command-message><command-args>https://example.com/pr/1</command-args>";
        let stripped = super::strip_command_markup(text);
        assert_eq!(stripped, "/brutal-review https://example.com/pr/1");
        assert_eq!(
            crate::util::classify_role("user", &stripped),
            crate::models::Role::Slash
        );
        assert_eq!(
            crate::util::slash_command_token(&stripped).as_deref(),
            Some("/brutal-review")
        );
    }

    #[test]
    fn strip_command_markup_normalizes_missing_leading_slash() {
        let text = "<command-name>effort</command-name><command-message>effort</command-message><command-args>max</command-args>";
        assert_eq!(super::strip_command_markup(text), "/effort max");
    }

    #[test]
    fn strip_command_markup_leaves_normal_messages() {
        assert_eq!(
            super::strip_command_markup("fix the bug in db.rs"),
            "fix the bug in db.rs"
        );
    }

    #[test]
    fn detects_tool_result_messages() {
        // role:user but a tool_result block → tool output, not a real user message.
        let tr = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "x", "content": "build failed: error"}]
        });
        assert!(super::is_tool_result(&tr));
        // A genuine user prompt is not a tool result.
        let user = json!({"role": "user", "content": [{"type": "text", "text": "fix the build"}]});
        assert!(!super::is_tool_result(&user));
        // Plain string content (no blocks) is not a tool result.
        let plain = json!({"role": "user", "content": "just text"});
        assert!(!super::is_tool_result(&plain));
    }

    #[test]
    fn tool_result_is_tagged_with_originating_tool_name() {
        use std::collections::HashMap;
        // An assistant turn issues a tool call — id -> name is recorded.
        let assistant = json!({
            "role": "assistant",
            "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "ls"}},
                {"type": "text", "text": "running it"}
            ]
        });
        let mut names = HashMap::new();
        super::collect_tool_use_names(&assistant, &mut names);
        assert_eq!(names.get("toolu_1").map(String::as_str), Some("Bash"));
        // The following user tool_result references that call by id, so it can be tagged.
        let result = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "a\nb"}]
        });
        let id = super::tool_result_id(&result).expect("tool_use_id present");
        assert_eq!(id, "toolu_1");
        assert_eq!(names.get(id).map(String::as_str), Some("Bash"));
        // A tool_result whose call id is unknown yields no tool name (rather than panicking).
        let orphan = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "missing", "content": "x"}]
        });
        let oid = super::tool_result_id(&orphan).expect("tool_use_id present");
        assert!(!names.contains_key(oid));
    }

    /// A `tool_result` block that omits `tool_use_id` is STILL a tool result — it must classify
    /// as `tool` (so it stays out of user/correction analytics); only the name tag is unavailable.
    /// Regression guard: an earlier "single scan" optimization collapsed this to
    /// `tool_result_id(...).is_some()`, which dropped the no-id case and mislabeled such messages
    /// as `user`.
    #[test]
    fn tool_result_without_id_is_still_a_tool_result() {
        let no_id = json!({
            "role": "user",
            "content": [{"type": "tool_result", "content": "done"}]
        });
        assert!(super::is_tool_result(&no_id));
        assert!(
            super::tool_result_id(&no_id).is_none(),
            "no id available, but the block still exists"
        );
        let with_id = json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "tu_1", "content": "done"}]
        });
        assert!(super::is_tool_result(&with_id));
        assert_eq!(super::tool_result_id(&with_id), Some("tu_1"));
    }

    /// Differential guard for the streaming-parse refactor (task #241): the streaming
    /// `BufReader` path must produce byte-identical `ParsedSession` output (messages,
    /// transcript_text, file_edits, AND the `line_count` metadata) versus the prior
    /// whole-file `fs::read_to_string` + `raw.lines()` implementation. The fixture
    /// deliberately exercises the line-count edge cases the streaming path could regress:
    /// a leading blank line, an interior blank line, a malformed (non-JSON) line, and a
    /// final line WITHOUT a trailing newline. `str::lines()` and `BufRead::lines()` count
    /// these identically (verified), so `line_count` must stay 7.
    #[test]
    fn streaming_parse_output_is_stable() {
        use super::ClaudeAdapter;
        use crate::models::Provider;
        use std::fs;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "11111111-2222-3333-4444-555555555555";
        let file = root.join(format!("{session_id}.jsonl"));
        // Line layout (7 lines, no trailing newline on the last):
        //   1: blank          2: user prompt       3: malformed JSON (skipped)
        //   4: assistant + Edit tool_use            5: user tool_result
        //   6: blank          7: final user prompt (NO trailing \n)
        let content = concat!(
            "\n",
            r#"{"sessionId":"11111111-2222-3333-4444-555555555555","type":"user","cwd":"/tmp/proj","timestamp":"2026-06-25T07:00:00.000Z","message":{"role":"user","content":[{"type":"text","text":"first prompt"}]}}"#,
            "\n",
            "{not valid json\n",
            r#"{"type":"assistant","timestamp":"2026-06-25T07:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":"on it"},{"type":"tool_use","id":"tu_1","name":"Edit","input":{"file_path":"/tmp/proj/a.rs","old_string":"x","new_string":"y"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-06-25T07:00:02.000Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"edited"}]}}"#,
            "\n",
            "\n",
            r#"{"type":"user","timestamp":"2026-06-25T07:00:03.000Z","message":{"role":"user","content":[{"type":"text","text":"second prompt"}]}}"#,
        );
        fs::write(&file, content).unwrap();

        let adapter = ClaudeAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);

        // line_count must reflect every physical line (incl. blanks + malformed), = 7.
        assert!(
            parsed
                .session
                .raw_metadata_json
                .as_deref()
                .unwrap()
                .contains("\"line_count\":7"),
            "line_count must be 7, got: {:?}",
            parsed.session.raw_metadata_json
        );
        // Full structural snapshot (source_path stripped — it is an absolute tempdir path).
        assert_eq!(parsed.session.provider, Provider::Claude);
        assert_eq!(parsed.session.message_count, Some(4));
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool", "user"]);
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec!["first prompt", "on it", "edited", "second prompt"]
        );
        // tool_result is tagged with the originating tool name.
        assert_eq!(parsed.messages[2].tool_name.as_deref(), Some("Edit"));
        // Transcript excludes the tool output; carries the conversation turns in order.
        assert!(parsed.transcript_text.contains("first prompt"));
        assert!(parsed.transcript_text.contains("on it"));
        assert!(parsed.transcript_text.contains("second prompt"));
        assert!(!parsed.transcript_text.contains("edited"));
        // The Edit tool_use produced exactly one file edit.
        assert_eq!(parsed.file_edits.len(), 1);
        assert_eq!(parsed.file_edits[0].file_path, "/tmp/proj/a.rs");
        assert_eq!(parsed.file_edits[0].tool, "Edit");
        assert_eq!(parsed.session.cwd.as_deref(), Some("/tmp/proj"));
        assert_eq!(parsed.session.title.as_deref(), Some("second prompt"));
    }

    /// Bytes that are not valid UTF-8 must never panic or abort the parse — they are decoded
    /// lossily (U+FFFD). A valid JSON line carrying a stray non-UTF-8 byte in a string value KEEPS
    /// its message (byte → U+FFFD); a line that is not valid JSON even after lossy decoding is
    /// simply skipped, like any other unparseable line. (Previously a single bad byte made
    /// `read_to_string`/`lines()` error and reduced the ENTIRE session to a minimal record.)
    #[test]
    fn non_utf8_bytes_are_recovered_lossily_not_dropped() {
        use super::ClaudeAdapter;
        use std::fs;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let file = root.join("66666666-7777-8888-9999-000000000000.jsonl");
        // Line 1: a valid claude user message whose text holds a raw 0xFF byte (invalid UTF-8).
        // Line 2: bytes that are not valid JSON even after lossy decoding (skipped, like garbage).
        let mut bytes = br#"{"type":"user","sessionId":"s","timestamp":"2026-06-01T10:00:00Z","cwd":"/p","message":{"role":"user","content":[{"type":"text","text":"hi "#.to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(br#" there"}]}}"#);
        bytes.push(b'\n');
        bytes.extend_from_slice(&[b'{', 0xFE, b'}', b'\n']);
        fs::write(&file, &bytes).unwrap();

        let adapter = ClaudeAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        // The valid line's message survived (not dropped by one bad byte); the byte became U+FFFD.
        assert_eq!(
            parsed.messages.len(),
            1,
            "the valid line is recovered, not lost to one bad byte"
        );
        let content = &parsed.messages[0].content;
        assert!(
            content.contains('\u{FFFD}'),
            "the invalid byte became the U+FFFD replacement char: {content:?}"
        );
        assert!(
            content.contains("hi") && content.contains("there"),
            "surrounding text is preserved: {content:?}"
        );
        assert_eq!(parsed.session.message_count, Some(1));
    }

    #[test]
    fn discovers_and_parses_claude_desktop_local_agent_audit_jsonl() {
        use super::ClaudeAdapter;
        use std::fs;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let root = temp.path().join("Claude/local-agent-mode-sessions");
        let parent = root.join("install-id/account-id");
        let session_dir = parent.join("local_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            parent.join("local_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.json"),
            r#"{
              "sessionId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
              "cliSessionId":"cli-session-1",
              "cwd":"/tmp/desktop-proj",
              "createdAt":"2026-03-29T19:14:00.000Z",
              "lastActivityAt":"2026-03-29T19:16:00.000Z",
              "title":"Desktop Agent Session",
              "initialMessage":"first desktop request"
            }"#,
        )
        .unwrap();
        fs::write(
            session_dir.join("audit.jsonl"),
            concat!(
                r#"{"type":"user","uuid":"u1","session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","message":{"role":"user","content":"first desktop request"},"_audit_timestamp":"2026-03-29T19:14:24.689Z"}"#,
                "\n",
                "{not json\n",
                r#"{"type":"assistant","uuid":"a1","session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","message":{"role":"assistant","content":[{"type":"text","text":"desktop answer"},{"type":"tool_use","id":"tu_1","name":"Write","input":{"file_path":"/tmp/desktop-proj/out.txt","content":"hello"}}]},"_audit_timestamp":"2026-03-29T19:14:30.000Z"}"#,
                "\n",
                r#"{"type":"user","uuid":"u2","session_id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"wrote file"}]},"_audit_timestamp":"2026-03-29T19:14:31.000Z"}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        assert_eq!(
            sources[0].path.file_name().and_then(|n| n.to_str()),
            Some("audit.jsonl")
        );

        let parsed = adapter.parse(&sources[0]);
        assert_eq!(
            parsed.session.id,
            "claude-desktop:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        );
        assert_eq!(parsed.session.provider, Provider::ClaudeDesktop);
        assert_eq!(
            parsed.session.provider_session_id,
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
        );
        assert_eq!(parsed.session.cwd.as_deref(), Some("/tmp/desktop-proj"));
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Desktop Agent Session")
        );
        assert_eq!(
            parsed.session.summary.as_deref(),
            Some("first desktop request")
        );
        assert_eq!(
            parsed.session.discovery_source,
            "claude-desktop-local-agent-audit-jsonl"
        );
        assert_eq!(
            parsed.session.parse_version,
            "claude-desktop-local-agent-v1"
        );
        assert_eq!(parsed.session.message_count, Some(3));
        assert!(
            parsed
                .session
                .raw_metadata_json
                .as_deref()
                .unwrap()
                .contains("\"malformed_line_count\":1"),
            "raw metadata should record skipped malformed lines: {:?}",
            parsed.session.raw_metadata_json
        );
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "tool"]);
        assert_eq!(parsed.messages[2].tool_name.as_deref(), Some("Write"));
        assert_eq!(parsed.file_edits.len(), 1);
        assert_eq!(parsed.file_edits[0].file_path, "/tmp/desktop-proj/out.txt");
        assert!(parsed.transcript_text.contains("first desktop request"));
        assert!(parsed.transcript_text.contains("desktop answer"));
        assert!(!parsed.transcript_text.contains("wrote file"));
    }

    #[test]
    fn claude_desktop_local_agent_without_sidecar_still_indexes_audit_messages() {
        use super::ClaudeAdapter;
        use std::fs;
        use tempfile::tempdir;

        let temp = tempdir().unwrap();
        let root = temp.path().join("local-agent-mode-sessions");
        let session_dir = root.join("install/account/local_ffffffff-1111-2222-3333-444444444444");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(
            session_dir.join("audit.jsonl"),
            concat!(
                r#"{"type":"user","session_id":"ffffffff-1111-2222-3333-444444444444","message":{"role":"user","content":"sidecar missing but parse me"},"_audit_timestamp":"2026-04-01T00:00:00.000Z"}"#,
                "\n",
                r#"{"type":"event","session_id":"ffffffff-1111-2222-3333-444444444444","payload":{"unknown":true},"_audit_timestamp":"2026-04-01T00:00:01.000Z"}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        assert_eq!(
            parsed.session.id,
            "claude-desktop:ffffffff-1111-2222-3333-444444444444"
        );
        assert_eq!(parsed.session.provider, Provider::ClaudeDesktop);
        assert_eq!(parsed.session.cwd, None);
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("sidecar missing but parse me")
        );
        assert_eq!(parsed.session.message_count, Some(1));
        assert_eq!(parsed.messages[0].content, "sidecar missing but parse me");
    }
}
