// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-FileCopyrightText: 2026 Thomas Funk
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{json, Value};

use crate::models::{
    ContentPartAuthorship, ContentPartOrigin, EditOp, FileEdit, MessageContentPart,
    MessageCorrelationAuthority, ParsedSession, Provider, SessionRecord, SourceFile,
};
use crate::providers::spawn::{self, SpawnOrigin};
use crate::util::{
    apply_user_role_authorship, extract_text, find_repo_root, format_transcript_line,
    minimal_record, normalize_path, parse_datetime, preview_from_text, substantive_text,
    truncate_for_display, RawMessage, UserRoleAuthorshipEvidence,
};

/// The workflow engine's own log, written beside the agent transcripts it spawned. It records
/// each agent's return value rather than a conversation, so it is metadata, not a session.
const WORKFLOW_JOURNAL_FILE: &str = "journal.jsonl";

/// Filename prefix claude gives every subagent transcript, in both layouts it writes them in.
const SUBAGENT_FILE_PREFIX: &str = "agent-";

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
                if path
                    .components()
                    .any(|component| component.as_os_str() == "memory")
                {
                    continue;
                }
                // Subagent transcripts — `<parent>/subagents/agent-<id>.jsonl` and the flat
                // `agent-<id>.jsonl` beside their parent — are sessions of their own; see
                // `ClaudeSpawn` for why, and how their identity is kept off the parent's row.
                // The workflow engine's `journal.jsonl` sits among them and records agent
                // return values rather than a conversation, so it is not a session.
                if path.file_name().and_then(|name| name.to_str()) == Some(WORKFLOW_JOURNAL_FILE)
                    && spawn::subagents_dir_origin(path).is_some()
                {
                    continue;
                }
                if let Ok(metadata) = entry.metadata() {
                    let source_kind = ClaudeSourceKind::from_path(path);
                    files.push(SourceFile {
                        provider: source_kind.provider(),
                        path: path.to_path_buf(),
                        mtime_ns: mtime_ns(&metadata),
                        size_bytes: metadata.len() as i64,
                    });
                    // Both transcript kinds that carry a JSON sidecar fold it in, so editing
                    // metadata beside an unchanged transcript still re-parses the session.
                    let sidecar = if is_claude_desktop_audit(path) {
                        claude_desktop_sidecar_path(path)
                    } else {
                        claude_subagent_sidecar_path(path)
                    };
                    if let (Some(sidecar), Some(source)) = (sidecar, files.last_mut()) {
                        fold_sidecar(source, &sidecar);
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
        // The session id identifies THIS file and is bound once. Desktop metadata is
        // authoritative when present; otherwise the first record carrying one wins.
        // Rebinding on every line lets any later record retarget the whole file, and the
        // `on conflict(id) do update` upsert in db.rs would then overwrite the row that id
        // belongs to instead of storing this session. See codex.rs for the same guard, where
        // an unguarded rebind cost 65 of 414 sessions.
        let mut session_id_bound = false;
        // A subagent transcript's records all carry its PARENT's sessionId, so no record in it
        // may bind this session's id. Hold the binding shut and compose the id after the loop,
        // where the parent is known; the parent becomes metadata, not this session's identity.
        let spawn = ClaudeSpawn::for_path(path);
        let subagent = spawn.is_subagent();
        if subagent {
            session_id_bound = true;
        }
        let sidecar = claude_subagent_sidecar(path);
        let mut parent_session_id: Option<String> = None;
        let mut agent_id: Option<String> = None;
        if let Some(session_id) = desktop.session_id.as_deref() {
            provider_session_id = session_id.to_string();
            session_id_bound = true;
        } else if source_kind == ClaudeSourceKind::DesktopLocalAgent {
            if let Some(session_id) = claude_desktop_session_id_from_path(path) {
                provider_session_id = session_id;
                session_id_bound = true;
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
            if !session_id_bound {
                // `session_id` keeps precedence over `sessionId` within one record, as before.
                if let Some(session_id) = value
                    .get("session_id")
                    .or_else(|| value.get("sessionId"))
                    .and_then(Value::as_str)
                {
                    provider_session_id = session_id.to_string();
                    session_id_bound = true;
                }
            }
            // For a subagent transcript the same field names the PARENT, which is worth
            // keeping: it is what links the subagent's work back to the session that spawned
            // it. Captured once, from the first record that carries each value.
            if subagent {
                if parent_session_id.is_none() {
                    parent_session_id = value
                        .get("sessionId")
                        .or_else(|| value.get("session_id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
                if agent_id.is_none() {
                    agent_id = value
                        .get("agentId")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned);
                }
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
            let event_id = value
                .get("uuid")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty());

            let mut role = value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut text = String::new();
            let mut tool_result = false;
            let mut tool_name: Option<String> = None;
            let mut tool_call_id: Option<String> = None;
            let mut mixed_user_content = None;

            if let Some(message) = value.get("message") {
                role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(role);
                text = extract_text(message);
                if role.as_deref() == Some("user") {
                    mixed_user_content = claude_mixed_user_content(message, subagent);
                }
                // Capture file-mutating tool calls before any text-based skip/continue,
                // so edits inside assistant turns with empty/skipped text are still recorded.
                collect_file_edits(message, timestamp, &mut file_edit_seq, &mut file_edits);
                collect_tool_use_names(message, &mut tool_use_names);
                append_tool_use_messages(message, timestamp, &mut messages);
                // One scan for the first tool_result block: `tool_result` is true when a block
                // EXISTS (even without a `tool_use_id`), and the name is tagged from that same
                // block's id when present. Was two scans (`is_tool_result` + `tool_result_id`).
                if let Some(block) = first_tool_result_block(message) {
                    tool_result = true;
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        tool_call_id = Some(id.to_string());
                        tool_name = tool_use_names.get(id).cloned();
                    }
                }
            } else if let Some(message) = value.get("content").and_then(Value::as_str) {
                text = message.to_string();
            }

            // Harness notices (Stop-hook feedback, PreToolUse blocks, local-command caveats,
            // task notifications) are the only record of what a hook told an agent and what it
            // did next, so they are stored and tagged rather than dropped. Every query excludes
            // them by default, so user prose and the analytics built on it are unaffected.
            if is_harness_notice(&value, &text) {
                let notice = text.trim();
                if !notice.is_empty() {
                    let mut raw_message = RawMessage::harness_notice(notice.to_string(), timestamp);
                    if let Some(event_id) = event_id {
                        raw_message = raw_message.with_native_event_identity(
                            MessageCorrelationAuthority::Anthropic,
                            event_id,
                        );
                    }
                    messages.push(raw_message);
                }
                continue;
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
                    if let Some(mixed) = mixed_user_content {
                        if created_at.is_none() {
                            created_at = timestamp;
                        }
                        if timestamp.is_some() {
                            updated_at = timestamp;
                        }
                        let mut raw_message =
                            RawMessage::message("user", mixed.content, timestamp, None);
                        if let Some(parts) = mixed.parts {
                            raw_message = raw_message.with_content_parts(parts);
                        }
                        if let Some(event_id) = event_id {
                            raw_message = raw_message.with_native_event_identity(
                                MessageCorrelationAuthority::Anthropic,
                                event_id,
                            );
                        }
                        messages.push(raw_message);
                        if !mixed.transcript_text.is_empty() {
                            transcript_lines.push(format_transcript_line(
                                "user",
                                timestamp,
                                &mixed.transcript_text,
                            ));
                        }
                        continue;
                    }
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    // Compaction digest → `compaction` role; tool output (claude records
                    // tool results as role:user) → `tool`. Both are searchable but excluded
                    // from user/correction/planning analytics and the human transcript
                    // (kept separate from the conversation, like other providers' tool output).
                    if is_compaction {
                        let mut raw_message =
                            RawMessage::message("compaction", text, timestamp, None);
                        if let Some(event_id) = event_id {
                            raw_message = raw_message.with_native_event_identity(
                                MessageCorrelationAuthority::Anthropic,
                                event_id,
                            );
                        }
                        messages.push(raw_message);
                        continue;
                    }
                    if tool_result {
                        let mut raw_message = RawMessage::tool_result_with_name(
                            tool_name,
                            text,
                            tool_call_id.as_deref(),
                            timestamp,
                        );
                        if let Some(event_id) = event_id {
                            raw_message = raw_message.with_native_event_identity(
                                MessageCorrelationAuthority::Anthropic,
                                event_id,
                            );
                        }
                        messages.push(raw_message);
                        continue;
                    }
                    if created_at.is_none() {
                        created_at = timestamp;
                    }
                    if timestamp.is_some() {
                        updated_at = timestamp;
                    }
                    let mut raw_message = RawMessage::message(
                        role.unwrap_or_default(),
                        text.clone(),
                        timestamp,
                        None,
                    );
                    if let Some(event_id) = event_id {
                        raw_message = raw_message.with_native_event_identity(
                            MessageCorrelationAuthority::Anthropic,
                            event_id,
                        );
                    }
                    messages.push(raw_message);
                    transcript_lines.push(format_transcript_line(
                        messages.last().map(RawMessage::role).unwrap_or("message"),
                        timestamp,
                        &text,
                    ));
                }
                _ => {}
            }
        }

        let first_user = messages
            .iter()
            .find(|message| message.role() == "user" && substantive_text(message.content()))
            .map(|message| message.content().to_string());
        let last_user = messages
            .iter()
            .rev()
            .find(|message| message.role() == "user" && substantive_text(message.content()))
            .map(|message| message.content().to_string());
        let title = sidecar
            .as_ref()
            .and_then(ClaudeSubagentSidecar::description)
            .map(|text| truncate_for_display(text, 100))
            .filter(|text| substantive_text(text))
            .or_else(|| desktop.title.clone())
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
        if let Some(sidecar) = sidecar.as_ref() {
            // Whole, not key-by-key: `toolUseId`, `spawnDepth`, `worktreePath`,
            // `worktreeBranch`, `model` and `stoppedByUser` all appear on live sidecars, and
            // enumerating them here would silently drop whichever key claude adds next.
            raw_metadata["subagent"] = sidecar.value.clone();
            raw_metadata["metadata_path"] = json!(normalize_path(&sidecar.path));
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

        // A subagent's identity is its parent's id plus what distinguishes the run under that
        // parent. Neither half alone works: the records name only the parent, and the agent id
        // repeats across parents. `Nested` takes the parent from the path because that is what
        // the run suffix is relative to — the two sources agreed on all 4,051 live transcripts,
        // so preferring the path costs nothing and keeps id and parent consistent by
        // construction.
        let provider = source_kind.provider();
        let (provider_session_id, parent_session_id) = match &spawn {
            ClaudeSpawn::TopLevel => (provider_session_id, None),
            ClaudeSpawn::Nested(origin) => {
                (origin.session_id(), Some(origin.parent_link(provider)))
            }
            ClaudeSpawn::BesideParent { run_suffix } => match parent_session_id {
                Some(parent) => (
                    format!("{parent}/{run_suffix}"),
                    Some(spawn::parent_link(provider, &parent)),
                ),
                // No record named a parent, so this run stands under its own name. Unique
                // among the siblings of one project directory, which is where these sit.
                None => (run_suffix.clone(), None),
            },
        };

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
            // Only a subagent run has a parent, resolved above.
            parent_session_id,
            // The sidecar's `agentType` is the name a reader recognizes; the agent id is the
            // fallback for the 2,243 transcripts that have no sidecar.
            agent_label: sidecar
                .as_ref()
                .and_then(ClaudeSubagentSidecar::agent_type)
                .map(ToOwned::to_owned)
                .or(agent_id),
        };

        let mut messages = crate::util::to_messages_with_tools_in_scope(messages, &session.id);
        apply_user_role_authorship(
            &mut messages,
            if subagent {
                UserRoleAuthorshipEvidence::AgentDelegationEvent
            } else {
                UserRoleAuthorshipEvidence::HumanInputEvent
            },
        );
        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages,
            file_edits,
        })
    }
}

struct ClaudeMixedUserContent {
    content: String,
    parts: Option<Vec<MessageContentPart>>,
    transcript_text: String,
}

fn claude_mixed_user_content(message: &Value, subagent: bool) -> Option<ClaudeMixedUserContent> {
    let blocks = message.get("content")?.as_array()?;
    let has_tool_result = blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
    let has_direct_text = blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) != Some("tool_result")
            && !extract_text(block).trim().is_empty()
    });
    if !(has_tool_result && has_direct_text) {
        return None;
    }

    let direct_authorship = if subagent {
        ContentPartAuthorship::Agent
    } else {
        ContentPartAuthorship::Human
    };
    let mut content = String::new();
    let mut parts: Vec<MessageContentPart> = Vec::new();
    let mut transcript_parts = Vec::new();
    for block in blocks {
        let text = extract_text(block);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let is_tool_result = block.get("type").and_then(Value::as_str) == Some("tool_result");
        if !content.is_empty() {
            content.push('\n');
            if let Some(previous) = parts.last_mut() {
                previous.end_char += 1;
            }
        }
        let start_char = content.chars().count();
        content.push_str(text);
        let end_char = content.chars().count();
        let (authorship, origin) = if is_tool_result {
            (
                ContentPartAuthorship::Generated,
                ContentPartOrigin::ToolPayload,
            )
        } else {
            transcript_parts.push(text.to_string());
            (direct_authorship, ContentPartOrigin::DirectInput)
        };
        parts.push(MessageContentPart {
            ordinal: parts.len() as u32,
            start_char,
            end_char,
            authorship,
            origin,
        });
    }
    // One linear pass over the retained partitions avoids fabricating `Mixed` when a structural
    // block (such as a tool reference) has no searchable text. No text or part is copied here.
    let has_distinct_authorship = parts.first().is_some_and(|first| {
        parts
            .iter()
            .skip(1)
            .any(|part| part.authorship != first.authorship)
    });
    Some(ClaudeMixedUserContent {
        content,
        parts: has_distinct_authorship.then_some(parts),
        transcript_text: transcript_parts.join("\n"),
    })
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
            Self::CodeJsonl => crate::util::provider_parse_version(Provider::Claude),
            Self::DesktopLocalAgent => crate::util::provider_parse_version(Provider::ClaudeDesktop),
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

/// Index assistant `tool_use` inputs as searchable tool messages. Tool results remain separate
/// rows, so users can search both what the agent called and what came back.
pub(crate) fn append_tool_use_messages(
    message: &Value,
    ts: Option<DateTime<Utc>>,
    out: &mut Vec<RawMessage>,
) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let Some(name) = block.get("name").and_then(Value::as_str) else {
            continue;
        };
        let args = block.get("input").cloned().unwrap_or(Value::Null);
        out.push(RawMessage::tool_call(
            name,
            args,
            block.get("id").and_then(Value::as_str),
            ts,
        ));
    }
}

/// The `tool_use_id` of the first `tool_result` block in a (role:user) message — the key
/// to look up the originating tool's name in the [`collect_tool_use_names`] map.
pub(crate) fn tool_result_id(message: &Value) -> Option<&str> {
    first_tool_result_block(message)
        .and_then(|block| block.get("tool_use_id").and_then(Value::as_str))
}

/// Whether this record is the harness talking to the agent rather than the user or model.
///
/// These were previously dropped outright, which also removed the only evidence of Stop-hook
/// denials and PreToolUse blocks: 82 of 82 "CANNOT STOP" records in one session carried
/// `isMeta` and none was searchable. They are now stored as `MessageKind::HarnessNotice` and
/// excluded from results by default, which keeps user prose and its analytics unchanged while
/// leaving the evidence reachable.
fn is_harness_notice(value: &Value, text: &str) -> bool {
    let normalized = text.trim();
    // Claude Code's own marker for bookkeeping injected as role:user: local-command caveats,
    // hook feedback ("Stop hook feedback: …"), system notices.
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return true;
    }
    // Background-task completion notices are injected as role:user (userType "external")
    // WITHOUT an isMeta flag, so the check above misses them.
    if normalized.starts_with("<task-notification") {
        return true;
    }
    // Local-command machinery recorded as role:user — slash-command stdout/stderr and caveats
    // (e.g. `/model` "Set model to …" stdout, `/compact` PreCompact hook output). Harness
    // output, not user prompts; counting them as prose would pollute corrections, repeat
    // mining, and user search. The empty type:"system" stdout variant is already ignored
    // (non user/assistant role); this catches the type:"user" content-string form.
    normalized.starts_with("<local-command-stdout>")
        || normalized.starts_with("<local-command-stderr>")
        || normalized.starts_with("<local-command-caveat>")
}

/// How a claude transcript relates to the session that spawned it.
///
/// Subagent transcripts are indexed as sessions in their own right, because what a subagent did
/// is worth searching — 4,051 of them on this machine against 858 top-level claude sessions.
/// They need an identity that is neither the parent's nor the bare agent id:
///
/// - Every record inside carries the PARENT's `sessionId`, never the run's own, so binding the
///   id from content points the row at the parent and the `on conflict(id) do update` upsert in
///   db.rs overwrites the parent with the subagent's content. Four parent rows were found
///   damaged this way on live data.
/// - The agent id is unique only within one parent: `agent-a0e105ee7f1fe2c65` appears under two
///   different parents here, so the bare id merges two runs into one row.
///
/// Both variants therefore resolve to a parent-qualified id; see [`SpawnOrigin`].
///
/// Decided from the path so discovery stays a directory walk without opening files. The
/// authoritative in-file marker is `isSidechain: true`, which every observed transcript carries
/// alongside an `agentId`; the parse reads the `agentId` for the label.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ClaudeSpawn {
    /// A top-level session: no parent, and its id comes from its own records as before.
    TopLevel,
    /// A run under `<parent-session-id>/subagents/`, where the path names both the parent and
    /// what distinguishes this run from its siblings. 4,047 of the 4,051 here.
    Nested(SpawnOrigin),
    /// A run stored directly beside its parent as `<project>/agent-<id>.jsonl`, 4 files here.
    /// The path names no parent, so the parent comes from the records instead.
    BesideParent { run_suffix: String },
}

impl ClaudeSpawn {
    fn for_path(path: &Path) -> Self {
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            return Self::TopLevel;
        };
        if !stem.starts_with(SUBAGENT_FILE_PREFIX) {
            return Self::TopLevel;
        }
        match spawn::subagents_dir_origin(path) {
            Some(origin) => Self::Nested(origin),
            None => Self::BesideParent {
                run_suffix: stem.to_string(),
            },
        }
    }

    fn is_subagent(&self) -> bool {
        !matches!(self, Self::TopLevel)
    }
}

/// The `agent-<id>.meta.json` claude writes beside a subagent transcript, holding the only
/// record of what the agent was asked to do and what kind of agent it was. Present for 1,808
/// of the 4,051 transcripts here, so everything it supplies has a fallback.
struct ClaudeSubagentSidecar {
    path: PathBuf,
    /// The sidecar object verbatim. Kept whole rather than destructured into named fields, so a
    /// key added upstream reaches `raw_metadata_json` without a change here; `agent_type` and
    /// `description` are read out of it for the typed label and the title.
    value: Value,
}

impl ClaudeSubagentSidecar {
    /// The name a reader recognizes for this kind of agent: `Explore`, `general-purpose`,
    /// `gsd-executor`. Codex's `agent_nickname` is the same idea.
    fn agent_type(&self) -> Option<&str> {
        self.value.get("agentType").and_then(Value::as_str)
    }

    /// The task the spawning session gave this agent, written by the spawner — a better title
    /// than the agent's own first turn, which is that same task restated at length.
    fn description(&self) -> Option<&str> {
        self.value.get("description").and_then(Value::as_str)
    }
}

/// Where a subagent transcript's sidecar would be: `agent-<id>.jsonl` → `agent-<id>.meta.json`.
/// `None` for anything that is not a subagent transcript, so ordinary sessions cost no stat.
fn claude_subagent_sidecar_path(path: &Path) -> Option<PathBuf> {
    ClaudeSpawn::for_path(path)
        .is_subagent()
        .then(|| path.with_extension("meta.json"))
}

fn claude_subagent_sidecar(path: &Path) -> Option<ClaudeSubagentSidecar> {
    let path = claude_subagent_sidecar_path(path)?;
    let raw = fs::read_to_string(&path).ok()?;
    let value = serde_json::from_str::<Value>(&raw).ok()?;
    Some(ClaudeSubagentSidecar { path, value })
}

fn mtime_ns(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_nanos() as i64)
        .unwrap_or_default()
}

/// Fold a sidecar's mtime and size into the transcript's [`SourceFile`], so editing metadata
/// stored beside an unchanged transcript still re-parses the session. Without this the indexer
/// compares the transcript alone, sees no change, and keeps the stale row.
fn fold_sidecar(source: &mut SourceFile, sidecar: &Path) {
    let Ok(metadata) = sidecar.metadata() else {
        return;
    };
    source.mtime_ns = source.mtime_ns.max(mtime_ns(&metadata));
    source.size_bytes = source.size_bytes.saturating_add(metadata.len() as i64);
}

fn should_skip_message(value: &Value, text: &str) -> bool {
    let normalized = text.trim();
    let _ = value;
    // Skip slash command invocations that carry no args — pure UI bookkeeping.
    // Invocations with args pass through; strip_command_markup keeps the command token and args.
    (normalized.contains("<command-name>")
        && tag_content(normalized, "command-args").trim().is_empty())
        || normalized.eq_ignore_ascii_case("resume cancelled")
}

#[cfg(test)]
mod tests {
    use super::{is_harness_notice, should_skip_message, ClaudeAdapter};
    use crate::models::Provider;
    use serde_json::json;
    use tempfile::tempdir;

    // These four cases were previously asserted against `should_skip_message`, i.e. that the
    // records were discarded. They are now stored as `MessageKind::HarnessNotice` and excluded
    // at query time instead, because discarding them also removed the only evidence of what a
    // hook told an agent: 82 of 82 "CANNOT STOP" records in one session were unsearchable.
    // Each test asserts the record is CLASSIFIED as a notice and NOT skipped, so a regression
    // to dropping fails here rather than silently emptying a category.

    /// A subagent transcript is worth searching, so it is indexed as a session of its own. It
    /// needs its own identity to be stored at all: every record inside carries the PARENT's
    /// `sessionId`, so binding from content points the row at the parent and the on-conflict
    /// upsert overwrites the parent's row with the subagent's content. Four such files were
    /// found colliding this way on live data, each reported as unindexed with no cause until
    /// `doctor --explain-unindexed` named the holder.
    #[test]
    fn a_subagent_transcript_keeps_its_own_id_and_records_its_parent() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("-tmp-proj");
        std::fs::create_dir_all(&project).unwrap();
        let parent = "7dc41785-a360-4c22-8db9-db6e37e9db23";
        std::fs::write(
            project.join(format!("{parent}.jsonl")),
            format!(
                r#"{{"type":"user","sessionId":"{parent}","cwd":"/tmp/proj","message":{{"role":"user","content":"the real session"}}}}
"#
            ),
        )
        .unwrap();
        // Real shape: isSidechain, an agentId, and the PARENT's sessionId on every record.
        std::fs::write(
            project.join("agent-0ccb8736.jsonl"),
            format!(
                r#"{{"type":"user","isSidechain":true,"agentId":"0ccb8736","sessionId":"{parent}","userType":"external","cwd":"/tmp/proj","message":{{"role":"user","content":"subagent turn"}}}}
"#
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 2, "both are indexed: {sources:#?}");

        let subagent = sources
            .iter()
            .find(|s| s.path.ends_with("agent-0ccb8736.jsonl"))
            .expect("the subagent transcript is discovered, not skipped");
        let parsed = adapter.parse(subagent);
        // Parent-qualified rather than the bare `agent-0ccb8736`: taking the parent's id
        // overwrites the parent's row, and the bare agent id merges two runs that share it
        // under different parents (one such pair among 4,051 live transcripts). Both halves
        // are needed, so the id carries both.
        assert_eq!(
            parsed.session.provider_session_id,
            format!("{parent}/agent-0ccb8736")
        );

        // Typed fields, not `raw_metadata_json` keys: the link has to be queryable, and every
        // provider produces this same shape. It holds the parent row's whole `id`, so the two
        // records show the same string and no prefix rule stands between them.
        assert_eq!(
            parsed.session.parent_session_id.as_deref(),
            Some(format!("claude:{parent}").as_str()),
            "the link back to the spawning session is what makes subagent work useful"
        );
        assert_eq!(parsed.session.agent_label.as_deref(), Some("0ccb8736"));
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Agent,
            "a user-role turn in a structurally identified subagent transcript is agent delegation"
        );

        // The parent still parses to itself, unaffected.
        let parent_source = sources
            .iter()
            .find(|s| s.path.ends_with(format!("{parent}.jsonl")))
            .unwrap();
        let parsed_parent = adapter.parse(parent_source);
        assert_eq!(parsed_parent.session.provider_session_id, parent);
        assert_eq!(
            parsed_parent.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Human
        );
    }

    #[test]
    fn mixed_user_text_and_tool_result_preserve_searchable_parts_and_human_transcript() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("mixed-session.jsonl");
        std::fs::write(
            &path,
            r#"{"type":"user","uuid":"mixed-1","sessionId":"mixed-session","message":{"role":"user","content":[{"type":"text","text":"before α"},{"type":"tool_result","tool_use_id":"tool-1","content":"output β"},{"type":"text","text":"after γ"}]}}"#,
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        assert_eq!(parsed.messages.len(), 1);
        let message = &parsed.messages[0];
        assert_eq!(message.role, crate::models::Role::User);
        assert_eq!(message.kind, crate::models::MessageKind::Conversation);
        assert_eq!(
            message.provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert_eq!(message.content, "before α\noutput β\nafter γ");
        assert_eq!(
            message
                .provenance
                .content_parts
                .iter()
                .map(|part| part.authorship)
                .collect::<Vec<_>>(),
            vec![
                crate::models::ContentPartAuthorship::Human,
                crate::models::ContentPartAuthorship::Generated,
                crate::models::ContentPartAuthorship::Human,
            ]
        );
        message
            .provenance
            .validate(&message.content)
            .expect("mixed parts exactly cover the Unicode-scalar content");
        assert!(
            message.tool_call_id.is_none(),
            "one mixed record cannot use one tool-call id as the identity of all parts"
        );
        assert!(parsed.transcript_text.contains("before α"));
        assert!(parsed.transcript_text.contains("after γ"));
        assert!(!parsed.transcript_text.contains("output β"));
    }

    #[test]
    fn subagent_tool_reference_without_text_does_not_fabricate_mixed_authorship() {
        let temp = tempdir().unwrap();
        let parent = "2eb8e351-15b9-4f06-b8f3-cf610843600a";
        let subagents = temp.path().join(parent).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        let path = subagents.join("agent-acompact-3d346a6bf6f19caa.jsonl");
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"user","isSidechain":true,"agentId":"acompact-3d346a6bf6f19caa","sessionId":"{parent}","message":{{"role":"user","content":[{{"type":"tool_result","content":[{{"type":"tool_reference","tool_name":"example"}}]}},{{"type":"text","text":"Tool loaded."}}]}}}}"#
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        assert_eq!(parsed.messages.len(), 1);
        let message = &parsed.messages[0];
        assert_eq!(message.content, "Tool loaded.");
        assert_eq!(message.role, crate::models::Role::User);
        assert_eq!(message.kind, crate::models::MessageKind::Conversation);
        assert_eq!(
            message.provenance.authorship,
            crate::models::MessageAuthorship::Agent,
            "a non-text tool reference contributes no searchable authored region"
        );
        assert!(
            message.provenance.content_parts.is_empty(),
            "a single surviving authorship must not be represented as mixed"
        );
        message
            .provenance
            .validate(&message.content)
            .expect("the exact live subagent shape must remain persistable");
    }

    /// Most subagent transcripts live in a `subagents` directory under their parent — 4,047 of
    /// the 4,051 on this machine — and were skipped outright by discovery.
    ///
    /// The id has to be parent-qualified rather than the bare `agent-<id>`, because that id is
    /// unique only within one parent: `agent-a0e105ee7f1fe2c65` appears under two different
    /// parents on live data. Under the bare id the `on conflict(id) do update` upsert in db.rs
    /// keeps whichever was written last and the other run is silently gone. This asserts which
    /// parent each row belongs to, not merely that the two ids differ — swapping them would
    /// preserve distinctness and pass a weaker check.
    #[test]
    fn two_subagents_sharing_an_agent_id_under_different_parents_both_survive() {
        let temp = tempdir().unwrap();
        let project = temp.path().join("-tmp-proj");
        let shared_agent = "a0e105ee7f1fe2c65";
        let parents = [
            "a2f3f693-e77f-4212-9e71-2b2331565fd4",
            "f2c5b7c7-c4bc-4ab3-bb21-7c381637b8ce",
        ];
        for parent in parents {
            let subagents = project.join(parent).join("subagents");
            std::fs::create_dir_all(&subagents).unwrap();
            std::fs::write(
                project.join(format!("{parent}.jsonl")),
                format!(
                    r#"{{"type":"user","sessionId":"{parent}","cwd":"/tmp/proj","message":{{"role":"user","content":"parent {parent}"}}}}
"#
                ),
            )
            .unwrap();
            // Real shape: isSidechain, the subagent's agentId, and the PARENT's sessionId.
            std::fs::write(
                subagents.join(format!("agent-{shared_agent}.jsonl")),
                format!(
                    r#"{{"type":"user","isSidechain":true,"agentId":"{shared_agent}","sessionId":"{parent}","userType":"external","cwd":"/tmp/proj","message":{{"role":"user","content":"work for {parent}"}}}}
"#
                ),
            )
            .unwrap();
        }

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        assert_eq!(
            sources.len(),
            4,
            "two parents and two subagent runs are all sessions: {sources:#?}"
        );

        for parent in parents {
            let source = sources
                .iter()
                .find(|source| {
                    source.path.ends_with(format!("agent-{shared_agent}.jsonl"))
                        && source.path.to_string_lossy().contains(parent)
                })
                .expect("each parent's subagent transcript is discovered");
            let parsed = adapter.parse(source);
            assert_eq!(
                parsed.session.provider_session_id,
                format!("{parent}/agent-{shared_agent}"),
                "the run must be identified by its parent plus what distinguishes it there"
            );
            assert_eq!(
                parsed.session.parent_session_id.as_deref(),
                Some(format!("claude:{parent}").as_str())
            );
            assert!(
                parsed
                    .transcript_text
                    .contains(&format!("work for {parent}")),
                "each row must hold its OWN transcript, not the other run's"
            );
        }

        // The two ids are what the db keys on, so state the consequence directly.
        let ids: Vec<String> = sources
            .iter()
            .map(|source| adapter.parse(source).session.id)
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "colliding ids overwrite rows: {ids:?}"
        );
    }

    /// Workflow agents nest one level deeper (`subagents/workflows/wf_<id>/agent-<id>.jsonl`).
    /// The workflow's own `journal.jsonl` sits beside them but is the workflow engine's record
    /// of agent return values, not a conversation, so it is not a session.
    #[test]
    fn a_workflow_agent_is_indexed_and_its_journal_is_not() {
        let temp = tempdir().unwrap();
        let parent = "77f26fc7-6ca3-4a98-a8b5-32f1963941ab";
        let workflow = temp
            .path()
            .join("-tmp-proj")
            .join(parent)
            .join("subagents")
            .join("workflows")
            .join("wf_4b4d88ab-f99");
        std::fs::create_dir_all(&workflow).unwrap();
        std::fs::write(
            workflow.join("agent-ae4f8452cb555e0bd.jsonl"),
            format!(
                r#"{{"type":"user","isSidechain":true,"agentId":"ae4f8452cb555e0bd","sessionId":"{parent}","cwd":"/tmp/proj","message":{{"role":"user","content":"review the diff"}}}}
"#
            ),
        )
        .unwrap();
        std::fs::write(
            workflow.join("journal.jsonl"),
            r#"{"agentId":"ae4f8452cb555e0bd","result":"done"}
"#,
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        assert_eq!(
            sources.len(),
            1,
            "only the agent transcript is a session: {sources:#?}"
        );
        let parsed = adapter.parse(&sources[0]);
        assert_eq!(
            parsed.session.provider_session_id,
            format!("{parent}/workflows/wf_4b4d88ab-f99/agent-ae4f8452cb555e0bd"),
            "the workflow is what distinguishes sibling runs, so it stays in the id"
        );
        assert_eq!(
            parsed.session.parent_session_id.as_deref(),
            Some(format!("claude:{parent}").as_str())
        );
    }

    /// A subagent transcript can have an `agent-<id>.meta.json` sidecar beside it — 1,808 of
    /// 4,051 do. It holds the only description of what the agent was asked to do and the only
    /// name for what kind of agent it was, so it supplies the title and the label. Keys beyond
    /// those two are kept verbatim rather than enumerated, so a key added upstream survives
    /// without a code change here.
    #[test]
    fn a_subagent_sidecar_supplies_the_title_the_label_and_its_other_keys() {
        let temp = tempdir().unwrap();
        let parent = "7e745098-c299-4cf5-bdbe-5cdb1fb5a62d";
        let subagents = temp.path().join("-tmp-proj").join(parent).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-a068fa115f7299f0c.jsonl"),
            format!(
                r#"{{"type":"user","isSidechain":true,"agentId":"a068fa115f7299f0c","sessionId":"{parent}","cwd":"/tmp/proj","message":{{"role":"user","content":"Read these files in full"}}}}
"#
            ),
        )
        .unwrap();
        std::fs::write(
            subagents.join("agent-a068fa115f7299f0c.meta.json"),
            r#"{"agentType":"Explore","description":"Read cship and starship config files in full","spawnDepth":1,"model":"claude-haiku-4-5-20251001"}"#,
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1, "the sidecar is metadata, not a session");
        let parsed = adapter.parse(&sources[0]);

        assert_eq!(
            parsed.session.agent_label.as_deref(),
            Some("Explore"),
            "agentType is the name a reader recognizes; the agentId is already in the session id"
        );
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Read cship and starship config files in full"),
            "the spawner's description of the task is a better title than the agent's own first turn"
        );
        let raw: serde_json::Value =
            serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(raw["subagent"]["spawnDepth"], json!(1));
        assert_eq!(raw["subagent"]["model"], json!("claude-haiku-4-5-20251001"));
        assert!(
            raw["metadata_path"]
                .as_str()
                .is_some_and(|path| path.ends_with("agent-a068fa115f7299f0c.meta.json")),
            "the sidecar path is recorded like the desktop sidecar's: {raw}"
        );
    }

    /// Only 1,808 of 4,051 transcripts have a sidecar, so the label falls back to the agent id
    /// the records carry rather than going empty.
    #[test]
    fn a_subagent_without_a_sidecar_labels_itself_with_its_agent_id() {
        let temp = tempdir().unwrap();
        let parent = "7e745098-c299-4cf5-bdbe-5cdb1fb5a62d";
        let subagents = temp.path().join("-tmp-proj").join(parent).join("subagents");
        std::fs::create_dir_all(&subagents).unwrap();
        std::fs::write(
            subagents.join("agent-a068fa115f7299f0c.jsonl"),
            format!(
                r#"{{"type":"user","isSidechain":true,"agentId":"a068fa115f7299f0c","sessionId":"{parent}","cwd":"/tmp/proj","message":{{"role":"user","content":"find the caller"}}}}
"#
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        let parsed = adapter.parse(&sources[0]);
        assert_eq!(
            parsed.session.agent_label.as_deref(),
            Some("a068fa115f7299f0c")
        );
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("find the caller"),
            "with no description to use, the title comes from the transcript as it always has"
        );
    }

    #[test]
    fn classifies_local_command_output_as_harness_notice() {
        // `/model`, `/compact`-hook etc. record their stdout/stderr as a role:user message
        // (type:"user", content is a bare string). Harness output, not a prompt.
        let value = json!({ "type": "user", "message": {"role": "user"} });
        for text in [
            "<local-command-stdout>Set model to Opus 4.8 and saved as your default</local-command-stdout>",
            "<local-command-stderr>boom</local-command-stderr>",
            "<local-command-caveat>note</local-command-caveat>",
        ] {
            assert!(is_harness_notice(&value, text), "not classified: {text}");
            assert!(!should_skip_message(&value, text), "discarded: {text}");
        }
        // A real prompt that merely mentions the tag name (not leading with it) stays prose.
        let prose = "what does <local-command-stdout> mean when it shows up in the logs";
        assert!(!is_harness_notice(&value, prose));
        assert!(!should_skip_message(&value, prose));
    }

    #[test]
    fn classifies_local_command_caveat_meta_messages_as_harness_notice() {
        let text = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>";
        let value = json!({
            "isMeta": true,
            "message": { "role": "user", "content": text }
        });
        assert!(is_harness_notice(&value, text));
        assert!(!should_skip_message(&value, text));
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
        assert!(!is_harness_notice(&value, "real prompt"));
        assert!(!should_skip_message(&value, "real prompt"));
    }

    #[test]
    fn classifies_hook_feedback_as_harness_notice() {
        // The exact record class that was unsearchable: hook output injected as a meta
        // role:user message with arbitrary text.
        let text = "Stop hook feedback: 🛑 CANNOT STOP — incomplete tasks: 1. #24";
        let value = json!({ "isMeta": true, "message": {"role": "user", "content": text} });
        assert!(is_harness_notice(&value, text));
        assert!(!should_skip_message(&value, text));
    }

    #[test]
    fn classifies_background_task_notifications_as_harness_notice() {
        // Background-task completion notices are injected as role:user with userType
        // "external" and NO isMeta flag, so they are matched on their leading tag instead.
        let text = "<task-notification>\n<task-id>bbawn9c36</task-id>\n<tool-use-id>toolu_01</tool-use-id>\n<output-file>/tmp/out.txt</output-file>\nAgent completed.";
        let value = json!({ "isMeta": false, "message": {"role": "user", "content": text} });
        assert!(is_harness_notice(&value, text));
        assert!(!should_skip_message(&value, text));
        // Leading whitespace must not defeat the match.
        let padded = format!("\n  {text}");
        assert!(is_harness_notice(&value, &padded));
        // A real prompt that merely mentions the word stays prose.
        let prose = "the task-notification format is confusing, can you explain it";
        assert!(!is_harness_notice(&value, prose));
        assert!(!should_skip_message(&value, prose));
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
        let text = "<command-name>/review-url</command-name><command-message>review-url</command-message><command-args>https://example.com/review/1</command-args>";
        let value = json!({ "isMeta": false });
        assert!(!should_skip_message(&value, text));
    }

    #[test]
    fn strip_command_markup_preserves_command_name_and_args() {
        // The command name is kept so the turn classifies as Role::Slash and planning
        // aggregation can recover `/review-url` via slash_command_token.
        let text = "<command-name>/review-url</command-name><command-message>review-url</command-message><command-args>https://example.com/review/1</command-args>";
        let stripped = super::strip_command_markup(text);
        assert_eq!(stripped, "/review-url https://example.com/review/1");
        assert_eq!(
            crate::util::classify_role("user", &stripped),
            crate::models::Role::Slash
        );
        assert_eq!(
            crate::util::slash_command_token(&stripped).as_deref(),
            Some("/review-url")
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

        let tool_call = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolCall)
            .expect("Claude tool_use indexed as a tool call");
        let tool_result = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolResult)
            .expect("Claude tool_result indexed as a tool result");
        assert_eq!(tool_call.tool_call_id.as_deref(), Some("tu_1"));
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("tu_1"));

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
        assert_eq!(parsed.session.message_count, Some(5));
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "tool", "assistant", "tool", "user"]);
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "first prompt",
                r#"{"args":{"file_path":"/tmp/proj/a.rs","new_string":"y","old_string":"x"},"kind":"tool_call","tool_name":"Edit"}"#,
                "on it",
                "edited",
                "second prompt"
            ]
        );
        // tool_result is tagged with the originating tool name.
        assert_eq!(parsed.messages[1].tool_name.as_deref(), Some("Edit"));
        assert_eq!(parsed.messages[3].tool_name.as_deref(), Some("Edit"));
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
            "claude-desktop-local-agent-v4"
        );
        assert_eq!(parsed.session.message_count, Some(4));
        let event_ids: Vec<Option<&str>> = parsed
            .messages
            .iter()
            .map(|message| {
                message
                    .provenance
                    .correlation_identity
                    .as_ref()
                    .map(|identity| identity.id.as_str())
            })
            .collect();
        assert_eq!(event_ids, vec![Some("u1"), None, Some("a1"), Some("u2")]);
        for identity in parsed
            .messages
            .iter()
            .filter_map(|message| message.provenance.correlation_identity.as_ref())
        {
            assert_eq!(
                identity.authority,
                crate::models::MessageCorrelationAuthority::Anthropic
            );
            assert_eq!(
                identity.scope,
                "claude-desktop:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"
            );
        }
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
        assert_eq!(roles, vec!["user", "tool", "assistant", "tool"]);
        assert_eq!(parsed.messages[1].tool_name.as_deref(), Some("Write"));
        assert_eq!(parsed.messages[3].tool_name.as_deref(), Some("Write"));
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
