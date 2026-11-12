// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-FileCopyrightText: 2026 Thomas Funk
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::files::{FileMutationPayload, PendingFileMutations};
use crate::hashing::sha256;
use crate::models::{
    ContentPartAuthorship, ContentPartOrigin, EditOp, FileEdit, InputDeliveryState,
    MessageAuthorship, MessageContentPart, MessageCorrelationAuthority, MessageCorrelationIdentity,
    MessageInputOrigin, ParsedSession, Provider, SessionRecord, SourceFile,
};
use crate::providers::{
    spawn::{self, SpawnOrigin},
    walk_roots, ProviderDiscovery, ProviderPathWarning,
};
use crate::util::{
    canonical_source_path, extract_text, find_repo_root, format_transcript_line, minimal_record,
    normalize_path, observe_session_end, observe_session_start, parse_datetime, preview_from_text,
    substantive_text, truncate_for_display, RawMessage, UserRoleAuthorshipEvidence,
};

/// The workflow engine's own log, written beside the agent transcripts it spawned. It records
/// each agent's return value rather than a conversation, so it is metadata, not a session.
const WORKFLOW_JOURNAL_FILE: &str = "journal.jsonl";

/// Filename prefix claude gives every subagent transcript, in both layouts it writes them in.
const SUBAGENT_FILE_PREFIX: &str = "agent-";

// Receipts can outlive their source; never resolve their already canonical path again.
pub(crate) fn claude_queue_scope(canonical_path: &str) -> String {
    format!("claude_queue:{canonical_path}")
}

/// Record types that are conversation, and the only ones whose records may become messages,
/// tool calls, tool results or file edits. `system` records reach only the harness-notice rule
/// (`is_harness_notice`); `queue-operation`, `attachment` and `last-prompt` have their own arms
/// in `parse_reader`'s dispatch.
///
/// Eligibility is decided by the record's `type`, never by whether it carries a `message` object:
/// a record may carry one without being a message. Since 2026-10-21 Claude Code writes
/// `api-request-blob` records, `{type, sessionId, hash, message: {role, content}}`, a stored copy
/// of each message of each API request. Taking the role from `message.role` indexed every copy
/// as another user or assistant turn and repeated its tool calls, tool results and file edits,
/// so harness wrappers and repeated prompts reached corrections, repeats and user search.
const MESSAGE_RECORD_TYPES: [&str; 3] = ["user", "assistant", "system"];

/// Record types Claude Code and Claude Desktop write that are deliberately not indexed: titles,
/// modes, request bookkeeping, file-history snapshots, progress and status events, and the
/// `api-request-blob` request copies whose content the conversation records already hold. A type
/// in neither list is counted and named in the session's parse warning, so a new record kind
/// appears in `aise doctor` instead of silently changing what is indexed.
const IGNORED_RECORD_TYPES: [&str; 27] = [
    "agent-name",
    "ai-title",
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "atis-latch",
    "command_lifecycle",
    "cost-state",
    "custom-title",
    "file-history-delta",
    "file-history-snapshot",
    "fork-context-ref",
    "frame-link",
    "mode",
    "permission-mode",
    "pr-link",
    "progress",
    "rate_limit_event",
    "relocated",
    "result",
    "session-owner",
    "started",
    // Conversation-summary records of Claude Code releases before this machine's earliest
    // transcript; no message content.
    "summary",
    "tool_progress",
    "tool_use_summary",
    "tools-used",
    "worktree-state",
];

/// `raw_metadata_json` key holding `{record type: count}` for records of unrecognized types,
/// which `Db::parser_health` aggregates across sessions.
pub(crate) const UNRECOGNIZED_RECORD_TYPES_KEY: &str = "unrecognized_record_types";

/// Records of types in neither `MESSAGE_RECORD_TYPES` nor `IGNORED_RECORD_TYPES`, by type name.
/// Bounded: at most `MAX_TYPES` names of at most `MAX_NAME_CHARS` characters each are kept, and
/// records of any further type are counted under one overflow name.
#[derive(Default)]
struct UnrecognizedRecordTypes {
    counts: std::collections::BTreeMap<String, usize>,
}

impl UnrecognizedRecordTypes {
    const MAX_TYPES: usize = 16;
    const MAX_NAME_CHARS: usize = 64;
    const OVERFLOW: &'static str = "(other types)";

    fn count(&mut self, record_type: Option<&str>) {
        let name: String = record_type
            .unwrap_or("(no type)")
            .chars()
            .take(Self::MAX_NAME_CHARS)
            .collect();
        let name = if self.counts.contains_key(&name) || self.counts.len() < Self::MAX_TYPES {
            name
        } else {
            Self::OVERFLOW.to_string()
        };
        *self.counts.entry(name).or_default() += 1;
    }

    fn warning(&self) -> Option<String> {
        if self.counts.is_empty() {
            return None;
        }
        Some(
            self.counts
                .iter()
                .map(|(name, count)| match count {
                    1 => format!("1 record of unrecognized type {name:?} was not indexed"),
                    count => {
                        format!("{count} records of unrecognized type {name:?} were not indexed")
                    }
                })
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// A queued prompt or notice found in an `attachment` record with
/// `attachment.type == "queued_command"`, held until the whole file is read.
///
/// Claude Code records a prompt typed while the agent was working as a `queue-operation`
/// enqueue, and when it is delivered as this attachment. Recent enqueues often omit `content`,
/// so the attachment can be the only copy of what the person typed. The structured
/// `attachment.prompt` is used, never the wrapped `rendered` text. A prompt already indexed from a
/// `queue-operation` enqueue or a `user` record of the same file is not added again: each indexed
/// copy of a text absorbs one attachment of that text, in file order, by the same
/// `strip_command_markup`-then-trim content digest the queue identity uses.
///
/// An enqueue records only that something was queued, not who sent it. When an enqueue's row
/// absorbs an attachment, the row takes the attachment's evidence: `commandMode == "prompt"`
/// with an `origin` gives the row that origin's authorship (`human` for a person), a prompt
/// without `origin` leaves the sender unknown, and any other mode makes the row a harness notice.
/// A `user` record that absorbs an attachment keeps its own record's evidence.
struct QueuedCommand {
    /// `messages.len()` and `transcript_lines.len()` when the attachment was read, so the row
    /// keeps its place in file order.
    position: usize,
    transcript_position: usize,
    digest: String,
    /// `commandMode == "prompt"`: input a person or agent submitted. Any other mode is not a
    /// typed prompt (task notifications, and peer or coordinator messages without a mode) and
    /// becomes a harness notice.
    prompt: bool,
    /// The sender named by `attachment.origin`, absent when the attachment names none.
    semantics: Option<ClaudeNativeMessageSemantics>,
    timestamp: Option<DateTime<Utc>>,
    message: RawMessage,
}

impl QueuedCommand {
    fn from_attachment(
        value: &Value,
        timestamp: Option<DateTime<Utc>>,
        event_id: Option<&str>,
        position: usize,
        transcript_position: usize,
    ) -> Option<Self> {
        let attachment = value.get("attachment")?;
        if attachment.get("type").and_then(Value::as_str) != Some("queued_command") {
            return None;
        }
        let prompt = attachment.get("prompt").and_then(Value::as_str)?;
        let is_prompt = attachment.get("commandMode").and_then(Value::as_str) == Some("prompt");
        let content = if is_prompt {
            strip_command_markup(prompt).trim().to_string()
        } else {
            prompt.trim().to_string()
        };
        if !substantive_text(&content) {
            return None;
        }
        let digest = sha256(content.as_bytes());
        // The sender is `attachment.origin`, the same field the SDK sets on a user record.
        let semantics = claude_native_message_semantics(attachment);
        let mut message = if is_prompt {
            queued_input_message(content, timestamp, semantics)
        } else {
            RawMessage::harness_notice(content, timestamp)
        };
        if let Some(event_id) = event_id {
            message = message
                .with_native_event_identity(MessageCorrelationAuthority::Anthropic, event_id);
        }
        Some(Self {
            position,
            transcript_position,
            digest,
            prompt: is_prompt,
            semantics,
            timestamp,
            message,
        })
    }

    /// Give each indexed copy of a queued text its attachment's evidence, and insert every
    /// queued command no other record of the file already indexed, in file order.
    ///
    /// `enqueued` maps the position in `messages` of each row built from a `queue-operation`
    /// enqueue to its line in `transcript_lines`.
    fn place(
        queued: Vec<Self>,
        enqueued: &HashMap<usize, usize>,
        messages: &mut Vec<RawMessage>,
        transcript_lines: &mut Vec<String>,
        created_at: &mut Option<DateTime<Utc>>,
        updated_at: &mut Option<DateTime<Utc>>,
    ) {
        if queued.is_empty() {
            return;
        }
        // Harness notices are stored with role `user`, so a notice already indexed from a
        // `user` record also absorbs its attachment copy.
        let mut indexed: HashMap<String, std::collections::VecDeque<usize>> = HashMap::new();
        for (position, message) in messages.iter().enumerate() {
            if message.role() == "user" {
                indexed
                    .entry(sha256(message.content().trim().as_bytes()))
                    .or_default()
                    .push_back(position);
            }
        }
        let mut retired_lines = false;
        let mut kept: Vec<Self> = Vec::new();
        for queued in queued {
            let Some(position) = indexed
                .get_mut(&queued.digest)
                .and_then(std::collections::VecDeque::pop_front)
            else {
                kept.push(queued);
                continue;
            };
            let Some(&line) = enqueued.get(&position) else {
                continue;
            };
            let row = &mut messages[position];
            row.fill_missing_ts(queued.timestamp);
            if !queued.prompt {
                row.retag_harness_notice();
                // Harness notices are not conversation, so they leave the session transcript.
                transcript_lines[line].clear();
                retired_lines = true;
            } else if let Some(semantics) = queued.semantics {
                row.set_authorship(semantics.content_authorship.into());
            }
        }

        // Back to front, so each recorded position still names the place it was read at.
        for queued in kept.into_iter().rev() {
            if queued.prompt {
                observe_session_start(created_at, queued.timestamp);
                observe_session_end(updated_at, queued.timestamp);
                transcript_lines.insert(
                    queued.transcript_position,
                    format_transcript_line("user", queued.timestamp, queued.message.content()),
                );
            }
            messages.insert(queued.position, queued.message);
        }
        if retired_lines {
            // Formatted transcript lines are never empty, so only the retired ones go. Removed
            // after the insertions above, whose recorded positions count every line.
            transcript_lines.retain(|line| !line.is_empty());
        }
    }
}

/// A prompt submitted through Claude Code's queue, from a `queue-operation` enqueue or a
/// `queued_command` attachment.
///
/// Legacy queue rows carry no sender or per-item origin. If the producer includes the SDK's
/// native origin, preserve it: a known generated/unclassified row must not become UserInput
/// merely because queue delivery is known, while direct/channel/scheduled input keeps the queue
/// origin and its submitted lifecycle evidence.
fn queued_input_message(
    content: String,
    timestamp: Option<DateTime<Utc>>,
    semantics: Option<ClaudeNativeMessageSemantics>,
) -> RawMessage {
    let raw_message = RawMessage::message("user", content, timestamp, None)
        .with_input_delivery_state(InputDeliveryState::Submitted);
    match semantics {
        Some(semantics) => {
            let raw_message = raw_message.with_authorship(semantics.content_authorship.into());
            if semantics.input_origin.is_some() {
                raw_message.with_input_origin(MessageInputOrigin::Queued)
            } else {
                raw_message
            }
        }
        None => raw_message
            .with_authorship(MessageAuthorship::Unknown)
            .with_input_origin(MessageInputOrigin::Queued),
    }
}

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
        self.discover_with_warnings().sources
    }

    pub(crate) fn discover_with_warnings(&self) -> ProviderDiscovery {
        let mut files = Vec::new();
        let mut walked = walk_roots(&self.roots, None);
        for entry in walked.entries {
            let path = &entry.path;
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
            let source_kind = ClaudeSourceKind::from_path(path);
            let sidecar = if is_claude_desktop_audit(path) {
                claude_desktop_sidecar_path(path)
            } else {
                claude_subagent_sidecar_path(path)
            };
            files.push(SourceFile {
                provider: source_kind.provider(),
                path: entry.path,
                mtime_ns: mtime_ns(&entry.metadata),
                size_bytes: entry.metadata.len() as i64,
            });
            // Both transcript kinds that carry a JSON sidecar fold it in, so editing
            // metadata beside an unchanged transcript still re-parses the session.
            if let (Some(sidecar), Some(source)) = (sidecar, files.last_mut()) {
                if let Some(warning) = fold_sidecar(source, &sidecar) {
                    walked.warnings.push(warning);
                }
            }
        }
        ProviderDiscovery {
            sources: files,
            warnings: walked.warnings,
        }
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
        let source_path = normalize_path(path);
        let desktop = match source_kind {
            ClaudeSourceKind::CodeJsonl => ClaudeDesktopMetadata::default(),
            ClaudeSourceKind::DesktopLocalAgent => claude_desktop_metadata(path),
        };
        let mut line_count: usize = 0;
        let mut malformed_line_count: usize = 0;
        let mut valid_record_count: usize = 0;
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
        let mut pending_file_mutations = PendingFileMutations::default();
        // tool_use_id -> tool name, so a later tool_result (which references the call by
        // id, not name) can be tagged with the tool it came from.
        let mut tool_use_names: HashMap<String, String> = HashMap::new();
        let mut queue_operation_counts: HashMap<String, usize> = HashMap::new();
        let mut queue_occurrences: HashMap<String, usize> = HashMap::new();
        let queue_scope = std::cell::OnceCell::new();
        let mut requires_full_parse_for_tail = false;
        // Queued prompts recorded only as attachments, placed after the loop once every
        // conversation and queue record of the file is known; see `QueuedCommand`.
        let mut queued_commands: Vec<QueuedCommand> = Vec::new();
        // Position in `messages` of each enqueue's row -> its line in `transcript_lines`.
        let mut enqueued: HashMap<usize, usize> = HashMap::new();
        let mut unrecognized = UnrecognizedRecordTypes::default();
        // What earlier parsers made of the records this one declines; see `RetractedRows`.
        let mut retracted_messages = Vec::new();
        let mut retracted_file_edits = Vec::new();
        let mut retracted_file_mutations = PendingFileMutations::default();
        let mut retracted_transcript_lines = Vec::new();
        // Assistant rows earlier parsers stored with `tool_use` input as text; see
        // `conversation_text`. Held apart because `retracted_messages` is borrowed per record.
        let mut earlier_assistant_rows = Vec::new();
        // Retain fixed-size digests rather than another copy of the transcript's bodies.
        let mut seen_records = std::collections::HashSet::new();

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
            valid_record_count += 1;
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

            let record_type = value.get("type").and_then(Value::as_str);
            // The one place that decides what a record may produce, by its type. A record of any
            // type outside `MESSAGE_RECORD_TYPES` yields no message, tool call, tool result or
            // file edit, whatever fields it carries; see that constant for why.
            let retract = match record_type {
                Some(kind) if MESSAGE_RECORD_TYPES.contains(&kind) => event_id.is_some_and(|id| {
                    !seen_records.insert((id.to_owned(), sha256(value.to_string().as_bytes())))
                }),
                Some("last-prompt") => {
                    if let Some(prompt) = value.get("lastPrompt").and_then(Value::as_str) {
                        let prompt = prompt.trim();
                        if substantive_text(prompt) {
                            last_prompt = Some(prompt.to_string());
                        }
                    }
                    continue;
                }
                Some("attachment") => {
                    if event_id.is_some_and(|id| {
                        !seen_records.insert((id.to_owned(), sha256(value.to_string().as_bytes())))
                    }) {
                        continue;
                    }
                    if let Some(queued) = QueuedCommand::from_attachment(
                        &value,
                        timestamp,
                        event_id,
                        messages.len(),
                        transcript_lines.len(),
                    ) {
                        // Whether it duplicates another record is decided after the loop, over
                        // the whole file, so a tail slice cannot decide it alone.
                        requires_full_parse_for_tail = true;
                        queued_commands.push(queued);
                    }
                    continue;
                }
                // Only enqueue records a submission; other queue operations establish neither a
                // new input nor a link proving delivery. Keep repeated submissions independently.
                Some("queue-operation") => {
                    let operation = value
                        .get("operation")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    *queue_operation_counts
                        .entry(operation.to_string())
                        .or_default() += 1;
                    if operation == "enqueue" {
                        requires_full_parse_for_tail = true;
                        if let Some(content) = value.get("content").and_then(Value::as_str) {
                            let content = strip_command_markup(content).trim().to_string();
                            if substantive_text(&content) {
                                let timestamp_key = value
                                    .get("timestamp")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                                    .or_else(|| timestamp.map(crate::util::stored_timestamp))
                                    .unwrap_or_else(|| "unknown-time".to_string());
                                // Timestamp alone is not stable when an earlier, different enqueue
                                // sharing that timestamp disappears from a shortened source. Include
                                // the content digest in the source-local bucket; identical repeated
                                // submissions still receive distinct occurrence suffixes.
                                let identity_key =
                                    format!("{timestamp_key}:{}", sha256(content.as_bytes()));
                                let occurrence =
                                    queue_occurrences.entry(identity_key.clone()).or_default();
                                let occurrence_id = *occurrence;
                                *occurrence += 1;
                                observe_session_start(&mut created_at, timestamp);
                                observe_session_end(&mut updated_at, timestamp);
                                let mut raw_message = queued_input_message(
                                    content.clone(),
                                    timestamp,
                                    claude_native_message_semantics(&value),
                                );
                                if let Some(event_id) = event_id {
                                    raw_message = raw_message.with_native_event_identity(
                                        MessageCorrelationAuthority::Anthropic,
                                        event_id,
                                    );
                                } else {
                                    raw_message = raw_message.with_correlation_identity(
                                        MessageCorrelationIdentity {
                                            authority: MessageCorrelationAuthority::SourceLocal,
                                            // Verified aliases must name the same physical source.
                                            scope: queue_scope
                                                .get_or_init(|| {
                                                    claude_queue_scope(&canonical_source_path(path))
                                                })
                                                .clone(),
                                            id: format!("{identity_key}#{occurrence_id}"),
                                        },
                                    );
                                }
                                // A notification the harness queued is not input, the same rule a
                                // `user` record of that text follows.
                                if is_harness_notice(&value, &content) {
                                    raw_message.retag_harness_notice();
                                } else {
                                    enqueued.insert(messages.len(), transcript_lines.len());
                                    transcript_lines
                                        .push(format_transcript_line("user", timestamp, &content));
                                }
                                messages.push(raw_message);
                            }
                        }
                    }
                    continue;
                }
                Some(kind) if IGNORED_RECORD_TYPES.contains(&kind) => true,
                other => {
                    unrecognized.count(other);
                    true
                }
            };

            if claude_tool_use_result(&value).is_some_and(|result| {
                result.get("answers").is_some_and(Value::is_object)
                    || result.get("response").is_some_and(Value::is_string)
            }) {
                requires_full_parse_for_tail = true;
            }

            // Claude Code and Claude Desktop SDK user records carry this optional, structural
            // origin metadata on the outer record. Absence is intentionally left unresolved so
            // the legacy session-level evidence below remains unchanged.
            let native_semantics = claude_native_message_semantics(&value);

            // A declined record runs through the same rendering into separate sinks, so what an
            // earlier parser stored from it can be recognised and removed; see `RetractedRows`.
            let (out, edits, pending) = if retract {
                (
                    &mut retracted_messages,
                    &mut retracted_file_edits,
                    &mut retracted_file_mutations,
                )
            } else {
                (&mut messages, &mut file_edits, &mut pending_file_mutations)
            };
            let mut role = record_type.map(str::to_string);
            let mut text = String::new();
            let mut tool_result = false;
            let mut tool_name: Option<String> = None;
            let mut tool_call_id: Option<String> = None;
            let mut mixed_user_content = None;

            if let Some(message) = value.get("message") {
                // `message.role` refines a user or assistant record. It never makes a record of
                // another type a turn; only the earlier rendering of a declined record used it.
                if retract || matches!(record_type, Some("user" | "assistant")) {
                    role = message
                        .get("role")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .or(role);
                }
                text = conversation_text(message);
                if role.as_deref() == Some("assistant") && !retract {
                    // Earlier parsers also read `tool_use` inputs as the turn's text, so a turn
                    // holding a `Write` call was stored with the written file as assistant prose.
                    // Render that row too, so reindex removes it; see `RetractedRows`.
                    let earlier = extract_text(message);
                    if earlier != text
                        && !is_harness_notice(&value, &earlier)
                        && !should_skip_message(&value, &earlier)
                    {
                        let earlier = strip_command_markup(&earlier);
                        let mut row =
                            RawMessage::message("assistant", earlier.clone(), timestamp, None);
                        if let Some(event_id) = event_id {
                            row = row.with_native_event_identity(
                                MessageCorrelationAuthority::Anthropic,
                                event_id,
                            );
                        }
                        earlier_assistant_rows.push(row);
                        retracted_transcript_lines.push(format_transcript_line(
                            "assistant",
                            timestamp,
                            &earlier,
                        ));
                    }
                }
                if role.as_deref() == Some("user") {
                    mixed_user_content = claude_mixed_user_content(
                        message,
                        &value,
                        subagent,
                        &tool_use_names,
                        true,
                        native_semantics,
                    );
                }
                // Capture file-mutating tool calls before any text-based skip/continue,
                // so edits inside assistant turns with empty/skipped text are still recorded.
                stage_file_edits(message, timestamp, pending);
                collect_file_edit_results(message, pending, edits);
                collect_tool_use_names(message, &mut tool_use_names);
                append_tool_use_messages(message, timestamp, out);
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
                    out.push(raw_message);
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
                        if !retract {
                            observe_session_start(&mut created_at, timestamp);
                            observe_session_end(&mut updated_at, timestamp);
                        }
                        let (mut raw_message, transcript_text) = mixed.into_raw_message(timestamp);
                        if let Some(event_id) = event_id {
                            raw_message = raw_message.with_native_event_identity(
                                MessageCorrelationAuthority::Anthropic,
                                event_id,
                            );
                        }
                        out.push(raw_message);
                        if !transcript_text.is_empty() {
                            let line = format_transcript_line("user", timestamp, &transcript_text);
                            if retract {
                                retracted_transcript_lines.push(line);
                            } else {
                                transcript_lines.push(line);
                            }
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
                        out.push(raw_message);
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
                        out.push(raw_message);
                        continue;
                    }
                    if !retract {
                        observe_session_start(&mut created_at, timestamp);
                        observe_session_end(&mut updated_at, timestamp);
                    }
                    let is_user_role = role.as_deref() == Some("user");
                    let mut raw_message = RawMessage::message(
                        role.unwrap_or_default(),
                        text.clone(),
                        timestamp,
                        None,
                    );
                    if is_user_role {
                        if let Some(semantics) = native_semantics {
                            raw_message =
                                raw_message.with_authorship(semantics.content_authorship.into());
                            if let Some(origin) = semantics.input_origin {
                                raw_message = raw_message.with_input_origin(origin);
                            }
                        }
                    }
                    if let Some(event_id) = event_id {
                        raw_message = raw_message.with_native_event_identity(
                            MessageCorrelationAuthority::Anthropic,
                            event_id,
                        );
                    }
                    let line = format_transcript_line(raw_message.role(), timestamp, &text);
                    if retract {
                        retracted_transcript_lines.push(line);
                    } else {
                        transcript_lines.push(line);
                    }
                    out.push(raw_message);
                }
                _ => {}
            }
        }

        retracted_messages.append(&mut earlier_assistant_rows);
        QueuedCommand::place(
            queued_commands,
            &enqueued,
            &mut messages,
            &mut transcript_lines,
            &mut created_at,
            &mut updated_at,
        );

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
            "valid_record_count": valid_record_count,
            "session_path": source_path,
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
        let has_queue_operations = !queue_operation_counts.is_empty();
        if has_queue_operations {
            raw_metadata["queue_operations"] = json!(queue_operation_counts);
        }
        if requires_full_parse_for_tail {
            raw_metadata["requires_full_parse_for_tail"] = json!(true);
        }
        if !unrecognized.counts.is_empty() {
            raw_metadata[UNRECOGNIZED_RECORD_TYPES_KEY] = json!(unrecognized.counts);
        }
        let raw_metadata_json = Some(serde_json::to_string(&raw_metadata)?);

        let parse_warning = [
            super::malformed_jsonl_warning(malformed_line_count),
            unrecognized.warning(),
        ]
        .into_iter()
        .flatten()
        .reduce(|first, second| format!("{first}; {second}"));

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

        let mut session = SessionRecord {
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
            source_path,
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

        let evidence = if subagent {
            UserRoleAuthorshipEvidence::AgentDelegationEvent
        } else {
            UserRoleAuthorshipEvidence::HumanInputEvent
        };
        let mut messages = crate::util::normalize_raw_messages_with_evidence(
            messages,
            Some(&session.id),
            evidence,
        );
        let mut retracted = crate::models::RetractedRows {
            messages: crate::util::normalize_raw_messages_with_evidence(
                retracted_messages,
                Some(&session.id),
                evidence,
            ),
            file_edits: retracted_file_edits,
            transcript: retracted_transcript_lines,
        };
        let mut seen_calls = std::collections::HashSet::new();
        messages.retain(|message| {
            if message.kind == crate::models::MessageKind::ToolCall {
                if let Some(id) = &message.tool_call_id {
                    if !seen_calls.insert((id.clone(), sha256(message.content.as_bytes()))) {
                        retracted.messages.push(message.clone());
                        return false;
                    }
                }
            }
            true
        });
        for (seq, message) in messages.iter_mut().enumerate() {
            message.seq = seq as i64;
        }
        session.message_count = Some(messages.len() as i64);
        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages,
            file_edits,
            retracted,
        })
    }
}

pub(crate) struct ClaudeMixedUserContent {
    content: String,
    parts: Option<Vec<MessageContentPart>>,
    transcript_text: String,
    has_interactive_answer: bool,
    has_direct_text: bool,
    native_authorship: Option<MessageAuthorship>,
    native_input_origin: Option<MessageInputOrigin>,
    tool_name: Option<String>,
    tool_call_id: Option<String>,
}

impl ClaudeMixedUserContent {
    pub(crate) fn into_raw_message(self, timestamp: Option<DateTime<Utc>>) -> (RawMessage, String) {
        let has_parts = self.parts.is_some();
        let mut raw = if self.has_direct_text {
            RawMessage::message("user", self.content, timestamp, self.tool_name)
                .with_tool_call_id(self.tool_call_id)
        } else {
            RawMessage::tool_result_with_name(
                self.tool_name,
                self.content,
                self.tool_call_id.as_deref(),
                timestamp,
            )
        };
        if let Some(parts) = self.parts {
            raw = raw.with_content_parts(parts);
        }
        if self.has_interactive_answer {
            raw = raw.with_input_origin(MessageInputOrigin::InteractiveAnswer);
            if !has_parts {
                raw =
                    raw.with_authorship(self.native_authorship.unwrap_or(MessageAuthorship::Human));
            }
        } else if self.has_direct_text {
            if let Some(origin) = self.native_input_origin {
                raw = raw.with_input_origin(origin);
            }
            if !has_parts {
                if let Some(authorship) = self.native_authorship {
                    raw = raw.with_authorship(authorship);
                }
            }
        }
        (raw, self.transcript_text)
    }
}

/// Exact source-native fields from Claude's SDK user-message envelope. This stays private: the
/// public model already has the authorship/origin vocabulary needed by search and persistence.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ClaudeNativeMessageSemantics {
    content_authorship: ContentPartAuthorship,
    content_origin: ContentPartOrigin,
    input_origin: Option<MessageInputOrigin>,
    allow_interactive_answers: bool,
}

fn claude_native_message_semantics(value: &Value) -> Option<ClaudeNativeMessageSemantics> {
    if let Some(is_synthetic) = value.get("isSynthetic") {
        match is_synthetic.as_bool() {
            Some(true) => {
                return Some(ClaudeNativeMessageSemantics {
                    content_authorship: ContentPartAuthorship::Generated,
                    content_origin: ContentPartOrigin::GeneratedSummary,
                    input_origin: None,
                    allow_interactive_answers: false,
                });
            }
            Some(false) => {}
            None => {
                return Some(ClaudeNativeMessageSemantics {
                    content_authorship: ContentPartAuthorship::Unknown,
                    content_origin: ContentPartOrigin::Unknown,
                    input_origin: None,
                    allow_interactive_answers: false,
                });
            }
        }
    }

    // SDK streams can interleave child messages in a main-session log. These fields identify
    // that child context; a missing origin must not inherit the main session's input fallback.
    if value.get("origin").is_none()
        && value.get("type").and_then(Value::as_str) == Some("user")
        && ["agent_id", "parent_tool_use_id"].iter().any(|key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|id| !id.trim().is_empty())
        })
    {
        return Some(ClaudeNativeMessageSemantics {
            content_authorship: ContentPartAuthorship::Agent,
            content_origin: ContentPartOrigin::DirectInput,
            input_origin: None,
            allow_interactive_answers: false,
        });
    }
    let origin = value.get("origin")?;
    let Some(origin) = origin.as_object() else {
        return Some(ClaudeNativeMessageSemantics {
            content_authorship: ContentPartAuthorship::Unknown,
            content_origin: ContentPartOrigin::Unknown,
            input_origin: None,
            allow_interactive_answers: false,
        });
    };
    let kind = origin.get("kind").and_then(Value::as_str);
    let direct_input = match kind {
        Some("human") | Some("channel") => true,
        Some("task-notification") => {
            origin.get("subkind").and_then(Value::as_str) == Some("scheduled-trigger")
        }
        _ => false,
    };
    let content_authorship = match kind {
        Some("human") => ContentPartAuthorship::Human,
        Some("channel") | Some("task-notification") if direct_input => {
            ContentPartAuthorship::Unknown
        }
        Some("peer") | Some("coordinator") => ContentPartAuthorship::Agent,
        Some("task-notification")
            if origin.get("subkind").and_then(Value::as_str) == Some("peer-send-message") =>
        {
            ContentPartAuthorship::Agent
        }
        Some("task-notification") | Some("auto-continuation") | Some("unclassified") => {
            ContentPartAuthorship::Generated
        }
        _ => ContentPartAuthorship::Unknown,
    };
    let content_origin = match kind {
        Some("human") | Some("channel") | Some("task-notification") if direct_input => {
            ContentPartOrigin::DirectInput
        }
        Some("auto-continuation") | Some("unclassified") | Some("task-notification") => {
            ContentPartOrigin::GeneratedSummary
        }
        Some("peer") | Some("coordinator") => ContentPartOrigin::DirectInput,
        _ => ContentPartOrigin::Unknown,
    };
    Some(ClaudeNativeMessageSemantics {
        content_authorship,
        content_origin,
        input_origin: direct_input.then_some(MessageInputOrigin::DirectConversation),
        allow_interactive_answers: direct_input,
    })
}

/// The prose of a `message`: every block's text except `tool_use` blocks. A tool call is indexed
/// as its own tool-call row, and a `Write` call's file text as a file edit; reading the call's
/// `input` as prose made a turn holding only a `Write` call an assistant message of the file.
fn conversation_text(message: &Value) -> String {
    match message.get("content").and_then(Value::as_array) {
        Some(blocks) => extract_text(&Value::Array(
            blocks
                .iter()
                .filter(|block| block.get("type").and_then(Value::as_str) != Some("tool_use"))
                .cloned()
                .collect(),
        )),
        None => extract_text(message),
    }
}

/// Claude Code uses camelCase while the SDK's documented user/replay envelope uses snake_case.
/// Keep this translation in one accessor so tail markers, attribution, and rendering agree.
fn claude_tool_use_result(value: &Value) -> Option<&Value> {
    value
        .get("toolUseResult")
        .or_else(|| value.get("tool_use_result"))
}

pub(crate) fn claude_mixed_user_content(
    message: &Value,
    value: &Value,
    subagent: bool,
    tool_use_names: &HashMap<String, String>,
    decode_native_answers: bool,
    native_semantics: Option<ClaudeNativeMessageSemantics>,
) -> Option<ClaudeMixedUserContent> {
    let blocks = message.get("content")?.as_array()?;
    let has_tool_result = blocks
        .iter()
        .any(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"));
    let has_direct_text = blocks.iter().any(|block| {
        block.get("type").and_then(Value::as_str) != Some("tool_result")
            && !extract_text(block).trim().is_empty()
    });
    if !has_tool_result {
        return None;
    }

    let direct_authorship = native_semantics
        .map(|semantics| semantics.content_authorship)
        .unwrap_or_else(|| {
            if subagent {
                ContentPartAuthorship::Agent
            } else {
                ContentPartAuthorship::Human
            }
        });
    let direct_origin = native_semantics
        .map(|semantics| semantics.content_origin)
        .unwrap_or(ContentPartOrigin::DirectInput);
    let mut content = String::new();
    let mut content_chars = 0;
    let mut parts: Vec<MessageContentPart> = Vec::new();
    let mut transcript_parts = Vec::new();
    let mut has_interactive_answer = false;
    let mut tool_result_count = 0;
    let mut tool_name = None;
    let mut tool_call_id = None;
    let allow_interactive_answers = decode_native_answers
        && native_semantics
            .map(|semantics| semantics.allow_interactive_answers)
            .unwrap_or(!subagent);
    let interactive_results = blocks
        .iter()
        .filter(|block| {
            block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .and_then(|id| tool_use_names.get(id))
                    .is_some_and(|name| is_ask_user_tool_name(name))
        })
        .count();
    for block in blocks {
        let is_tool_result = block.get("type").and_then(Value::as_str) == Some("tool_result");
        if is_tool_result {
            tool_result_count += 1;
            if tool_result_count == 1 {
                tool_call_id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                tool_name = tool_call_id
                    .as_deref()
                    .and_then(|id| tool_use_names.get(id))
                    .cloned();
            } else {
                // One aggregate record may contain several independent results. A first
                // call's id/name would misdescribe the other blocks, so keep the row's
                // content but clear the ambiguous tool metadata.
                tool_call_id = None;
                tool_name = None;
            }
        }
        let text = extract_text(block).trim().to_owned();
        let linked_answer = is_tool_result
            && allow_interactive_answers
            && interactive_results == 1
            && block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .and_then(|id| tool_use_names.get(id))
                .is_some_and(|name| is_ask_user_tool_name(name));
        let (text, attributed_answer) = if linked_answer {
            interactive_answer_content(
                value,
                text,
                native_semantics
                    .map(|semantics| semantics.content_authorship)
                    .unwrap_or(ContentPartAuthorship::Human),
            )
        } else {
            (text, None)
        };
        if text.is_empty() {
            continue;
        }
        if !content.is_empty() {
            content.push('\n');
            let separator_start = content_chars;
            content_chars += 1;
            if let Some(previous) = parts.last_mut().filter(|part| {
                part.authorship == ContentPartAuthorship::Generated
                    && part.origin == ContentPartOrigin::ToolPayload
            }) {
                previous.end_char += 1;
            } else {
                parts.push(content_part(
                    parts.len() as u32,
                    separator_start,
                    content_chars,
                    ContentPartAuthorship::Generated,
                    ContentPartOrigin::ToolPayload,
                ));
            }
        }
        let start_char = content_chars;
        content.push_str(&text);
        content_chars += text.chars().count();
        let end_char = content_chars;
        if attributed_answer.is_some() {
            has_interactive_answer = true;
        }
        if let Some(attribution) = attributed_answer {
            match attribution {
                InteractiveAnswerAttribution::Whole => parts.push(MessageContentPart {
                    ordinal: parts.len() as u32,
                    start_char,
                    end_char,
                    authorship: direct_authorship,
                    origin: ContentPartOrigin::DirectInput,
                }),
                InteractiveAnswerAttribution::Mixed(local_parts) => {
                    for mut part in local_parts {
                        part.ordinal = parts.len() as u32;
                        part.start_char += start_char;
                        part.end_char += start_char;
                        parts.push(part);
                    }
                }
            }
        } else if is_tool_result {
            if let Some(previous) = parts.last_mut().filter(|part| {
                part.authorship == ContentPartAuthorship::Generated
                    && part.origin == ContentPartOrigin::ToolPayload
            }) {
                previous.end_char = end_char;
            } else {
                parts.push(content_part(
                    parts.len() as u32,
                    start_char,
                    end_char,
                    ContentPartAuthorship::Generated,
                    ContentPartOrigin::ToolPayload,
                ));
            }
        } else {
            transcript_parts.push(text.to_string());
            parts.push(MessageContentPart {
                ordinal: parts.len() as u32,
                start_char,
                end_char,
                authorship: direct_authorship,
                origin: direct_origin,
            });
        }
    }
    if content.is_empty() {
        return None;
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
        has_interactive_answer,
        has_direct_text,
        native_authorship: native_semantics.map(|semantics| semantics.content_authorship.into()),
        native_input_origin: native_semantics.and_then(|semantics| semantics.input_origin),
        tool_name,
        tool_call_id,
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

/// Stage file-mutating calls by native id until their corresponding result proves completion.
fn stage_file_edits(
    message: &Value,
    ts: Option<DateTime<Utc>>,
    pending: &mut PendingFileMutations,
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
        pending.stage(
            block.get("id").and_then(Value::as_str),
            ts,
            name,
            tool_use_payload(name, block.get("input")),
        );
    }
}

/// Extract mutations immediately for provider formats whose edit record is authoritative itself.
/// Claude Code does not use this path: it stages calls above until a correlated result arrives.
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
        let Some((file_path, new_content, edits)) = tool_use_payload(name, block.get("input"))
        else {
            continue;
        };
        out.push(FileEdit {
            seq: *next_seq,
            ts,
            tool: name.to_string(),
            file_name: crate::util::file_basename(&file_path),
            file_path,
            new_content,
            edits,
        });
        *next_seq += 1;
    }
}

fn collect_file_edit_results(
    message: &Value,
    pending: &mut PendingFileMutations,
    out: &mut Vec<FileEdit>,
) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        pending.finish(
            block.get("tool_use_id").and_then(Value::as_str),
            // Observed Claude Code records use both `is_error:false` and an omitted flag for
            // successful results; only `is_error:true` is an explicit failure.
            block.get("is_error").and_then(Value::as_bool) != Some(true),
            out,
        );
    }
}

/// Map a single file-mutating tool call to `(file_path, full_content?, edits)`.
/// `Write` yields a full-content snapshot; `Edit`/`MultiEdit` yield delta ops (carrying
/// the `replace_all` flag); `NotebookEdit` is recorded (path only) so it appears in
/// history/cross-ref, but carries no replayable delta (cell reconstruction is out of scope).
fn tool_use_payload(name: &str, input: Option<&Value>) -> Option<FileMutationPayload> {
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

fn is_ask_user_tool_name(name: &str) -> bool {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-'))
        .flat_map(char::to_lowercase)
        .eq("askuserquestion".chars())
}

enum InteractiveAnswerAttribution {
    Whole,
    Mixed(Vec<MessageContentPart>),
}

/// Keep native answers findable even when the display omits them. Existing wrapper bytes stay
/// generated; only unrepresented decoded values are appended, with generated separators.
/// With A answers, B display bytes and N native payload bytes: expected O(N + A*B + A log A)
/// time, O(N + B + A) peak state and O(B + T + A) retained output for T answer bytes.
fn interactive_answer_content(
    value: &Value,
    mut content: String,
    answer_authorship: ContentPartAuthorship,
) -> (String, Option<InteractiveAnswerAttribution>) {
    let attribution = interactive_answer_attribution(value, &content, answer_authorship);
    let mut parts = match attribution {
        Some(InteractiveAnswerAttribution::Whole) => vec![content_part(
            0,
            0,
            content.chars().count(),
            answer_authorship,
            ContentPartOrigin::DirectInput,
        )],
        Some(InteractiveAnswerAttribution::Mixed(parts)) => parts,
        None if !content.is_empty() => vec![content_part(
            0,
            0,
            content.chars().count(),
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        )],
        None => Vec::new(),
    };
    let mut represented = HashMap::<String, usize>::new();
    for part in parts
        .iter()
        .filter(|part| part.authorship == answer_authorship)
    {
        let text = content
            .chars()
            .skip(part.start_char)
            .take(part.end_char - part.start_char)
            .collect();
        *represented.entry(text).or_default() += 1;
    }
    let result = claude_tool_use_result(value);
    let values: Vec<&str> = if let Some(response) = result
        .and_then(|result| result.get("response"))
        .and_then(Value::as_str)
    {
        vec![response]
    } else {
        let order: HashMap<&str, usize> = result
            .and_then(|result| result.get("questions"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, question)| {
                question
                    .get("question")
                    .and_then(Value::as_str)
                    .map(|question| (question, index))
            })
            .collect();
        let mut answers: Vec<_> = result
            .and_then(|result| result.get("answers"))
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|answers| answers.iter())
            .filter_map(|(question, answer)| {
                answer.as_str().map(|answer| (question.as_str(), answer))
            })
            .collect();
        answers
            .sort_by_cached_key(|(question, _)| order.get(question).copied().unwrap_or(usize::MAX));
        answers.into_iter().map(|(_, answer)| answer).collect()
    };
    let mut cursor = content.chars().count();
    for answer in values.into_iter().filter(|answer| !answer.is_empty()) {
        if let Some(count) = represented.get_mut(answer) {
            if *count > 0 {
                *count -= 1;
                continue;
            }
        }
        if !content.is_empty() {
            content.push('\n');
            parts.push(content_part(
                parts.len() as u32,
                cursor,
                cursor + 1,
                ContentPartAuthorship::Generated,
                ContentPartOrigin::ToolPayload,
            ));
            cursor += 1;
        }
        content.push_str(answer);
        let end = cursor + answer.chars().count();
        parts.push(content_part(
            parts.len() as u32,
            cursor,
            end,
            answer_authorship,
            ContentPartOrigin::DirectInput,
        ));
        cursor = end;
    }
    let has_input = parts
        .iter()
        .any(|part| part.authorship == answer_authorship);
    let has_generated = parts
        .iter()
        .any(|part| part.authorship == ContentPartAuthorship::Generated);
    let attribution = if !has_input {
        None
    } else if has_generated {
        Some(InteractiveAnswerAttribution::Mixed(parts))
    } else {
        Some(InteractiveAnswerAttribution::Whole)
    };
    (content, attribution)
}

/// Attribute a tool-result wrapper using only the structured answer values belonging to an
/// earlier AskUserQuestion call. Unknown wrapper text remains generated; absent or unmappable
/// answers return `None` so the normal tool-result classification remains unchanged.
/// For A answers, B wrapper bytes and P parts, matching costs O(A*B + P log P); scalar
/// conversion costs O(B + P), with O(P + longest pair) temporary memory and no wrapper copy.
/// shortcut: scan per answer; use multi-pattern matching if large answer maps measure costly.
fn interactive_answer_attribution(
    value: &Value,
    content: &str,
    answer_authorship: ContentPartAuthorship,
) -> Option<InteractiveAnswerAttribution> {
    let result = claude_tool_use_result(value)?;
    if let Some(response) = result.get("response").and_then(Value::as_str) {
        if response.is_empty() {
            return None;
        }
        if content == response {
            return Some(InteractiveAnswerAttribution::Whole);
        }
        const PREFIX: &str = "The user responded: ";
        if content.strip_prefix(PREFIX) != Some(response) {
            return None;
        }
        let start = PREFIX.chars().count();
        return Some(InteractiveAnswerAttribution::Mixed(vec![
            content_part(
                0,
                0,
                start,
                ContentPartAuthorship::Generated,
                ContentPartOrigin::ToolPayload,
            ),
            content_part(
                1,
                start,
                content.chars().count(),
                answer_authorship,
                ContentPartOrigin::DirectInput,
            ),
        ]));
    }
    let answers = result.get("answers").and_then(Value::as_object)?;
    if answers.len() == 1 {
        if let Some(answer) = answers.values().next().and_then(Value::as_str) {
            if !answer.is_empty() && content == answer {
                return Some(InteractiveAnswerAttribution::Whole);
            }
        }
    }
    // Claude's wrapper is structurally `"question"="answer"`; do not search for an answer
    // value in arbitrary wrapper prose, because the same word can occur in the question or in
    // generated text. Repeated pairs are ambiguous and remain generated.
    let mut matches = answers
        .iter()
        .filter_map(|(question, answer)| {
            let answer = answer.as_str().filter(|answer| !answer.is_empty())?;
            let pair = format!(r#""{question}"="{answer}""#);
            let mut occurrences = content.match_indices(&pair);
            let (pair_start, _) = occurrences.next()?;
            if occurrences.next().is_some() {
                return None;
            }
            let answer_start = pair_start + question.len() + 4;
            Some((answer_start, answer_start + answer.len()))
        })
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return None;
    }
    matches.sort_unstable_by_key(|(start, _)| *start);
    if matches.windows(2).any(|window| window[1].0 < window[0].1) {
        return None;
    }
    // Walk disjoint slices once; a byte-to-character table would allocate a machine word for
    // every input byte even though only the part boundaries need scalar coordinates.
    let mut cursor = 0usize;
    let mut cursor_char = 0usize;
    let mut parts = Vec::new();
    for (start, end) in matches {
        let start_char = cursor_char + content[cursor..start].chars().count();
        let end_char = start_char + content[start..end].chars().count();
        if start > cursor {
            parts.push(content_part(
                parts.len() as u32,
                cursor_char,
                start_char,
                ContentPartAuthorship::Generated,
                ContentPartOrigin::ToolPayload,
            ));
        }
        parts.push(content_part(
            parts.len() as u32,
            start_char,
            end_char,
            answer_authorship,
            ContentPartOrigin::DirectInput,
        ));
        cursor = end;
        cursor_char = end_char;
    }
    if cursor < content.len() {
        parts.push(content_part(
            parts.len() as u32,
            cursor_char,
            cursor_char + content[cursor..].chars().count(),
            ContentPartAuthorship::Generated,
            ContentPartOrigin::ToolPayload,
        ));
    }
    let has_input = parts
        .iter()
        .any(|part| part.authorship == answer_authorship);
    let has_generated = parts
        .iter()
        .any(|part| part.authorship == ContentPartAuthorship::Generated);
    if !has_input {
        return None;
    }
    if has_generated {
        Some(InteractiveAnswerAttribution::Mixed(parts))
    } else {
        Some(InteractiveAnswerAttribution::Whole)
    }
}

fn content_part(
    ordinal: u32,
    start_char: usize,
    end_char: usize,
    authorship: ContentPartAuthorship,
    origin: ContentPartOrigin,
) -> MessageContentPart {
    MessageContentPart {
        ordinal,
        start_char,
        end_char,
        authorship,
        origin,
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
fn fold_sidecar(source: &mut SourceFile, sidecar: &Path) -> Option<ProviderPathWarning> {
    match sidecar.metadata() {
        Ok(metadata) => {
            source.mtime_ns = source.mtime_ns.max(mtime_ns(&metadata));
            source.size_bytes = source.size_bytes.saturating_add(metadata.len() as i64);
            None
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => Some(ProviderPathWarning {
            path: sidecar.to_path_buf(),
            operation: "read_sidecar",
            message: error.to_string(),
        }),
    }
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
    use serde_json::{json, Value};
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
{{"type":"queue-operation","operation":"enqueue","sessionId":"{parent}","timestamp":"2026-06-17T00:00:00Z","content":"subagent queued turn"}}
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
        assert_eq!(parsed.messages[1].content, "subagent queued turn");
        assert_eq!(
            parsed.messages[1].provenance.authorship,
            crate::models::MessageAuthorship::Unknown,
            "the queue record carries no author receipt, even in a subagent transcript"
        );
        assert_eq!(
            parsed.messages[1].provenance.input_origin,
            Some(crate::models::MessageInputOrigin::Queued)
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
    fn subagent_interactive_answers_do_not_infer_human_authorship_from_the_tool_name() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("parent/subagents/agent-answer.jsonl");
        let records = concat!(
            r#"{"type":"assistant","sessionId":"parent","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-1","name":"AskUserQuestion","input":{}}]}}"#,
            "\n",
            r#"{"type":"user","sessionId":"parent","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"yes"}]},"toolUseResult":{"answers":{"Proceed?":"yes"}}}"#,
            "\n"
        );
        let parsed = ClaudeAdapter::new(vec![])
            .parse_reader(std::io::Cursor::new(records), &path)
            .unwrap();
        assert!(parsed.messages.iter().all(
            |message| message.provenance.authorship != crate::models::MessageAuthorship::Human
        ));
        assert!(parsed
            .messages
            .iter()
            .flat_map(|message| &message.provenance.content_parts)
            .all(|part| part.authorship != crate::models::ContentPartAuthorship::Human));
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
            message.provenance.content_parts[0].end_char,
            "before α".chars().count(),
            "inserted separators are generated, not part of the submitted text"
        );
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
        assert_eq!(
            message.tool_call_id.as_deref(),
            Some("tool-1"),
            "a single tool relation is retained independently of the source event identity"
        );
        assert!(parsed.transcript_text.contains("before α"));
        assert!(parsed.transcript_text.contains("after γ"));
        assert!(!parsed.transcript_text.contains("output β"));
    }

    #[test]
    fn mixed_direct_interactive_and_ordinary_tool_results_preserve_each_origin() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("mixed-interactive-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","sessionId":"mixed-interactive","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-mixed","name":"AskUserQuestion","input":{}},{"type":"tool_use","id":"ordinary","name":"Read","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"mixed-interactive","message":{"role":"user","content":[{"type":"text","text":"direct request"},{"type":"tool_result","tool_use_id":"ordinary","content":"ordinary output"},{"type":"tool_result","tool_use_id":"ask-mixed","content":"Your questions have been answered: \"Did you say yes?\"=\"yes\"."}]},"toolUseResult":{"answers":{"Did you say yes?":"yes"}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let message = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("direct request"))
            .expect("mixed user record");
        assert_eq!(
            message.provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert_eq!(
            message.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        let human_parts: Vec<_> = message
            .provenance
            .content_parts
            .iter()
            .filter(|part| part.authorship == crate::models::ContentPartAuthorship::Human)
            .collect();
        assert_eq!(human_parts.len(), 2, "direct text plus answer value");
        assert!(human_parts.iter().any(|part| {
            let text: String = message
                .content
                .chars()
                .skip(part.start_char)
                .take(part.end_char - part.start_char)
                .collect();
            text == "yes"
        }));
        assert!(message.provenance.content_parts.iter().any(|part| {
            part.authorship == crate::models::ContentPartAuthorship::Generated
                && part.origin == crate::models::ContentPartOrigin::ToolPayload
        }));
        message.provenance.validate(&message.content).unwrap();
    }

    #[test]
    fn later_ask_user_tool_result_is_attributed_without_direct_text() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("later-interactive-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","sessionId":"later-interactive","message":{"role":"assistant","content":[{"type":"tool_use","id":"ordinary","name":"Read","input":{}},{"type":"tool_use","id":"ask-later","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"later-interactive","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ordinary","content":"ordinary output"},{"type":"tool_result","tool_use_id":"ask-later","content":"User has answered: \"Quality?\"=\"yes\"."}]},"toolUseResult":{"answers":{"Quality?":"yes"}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolResult)
            .expect("the aggregate tool-result row remains searchable");
        assert_eq!(
            answer.provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert!(answer.provenance.content_parts.iter().any(|part| {
            part.authorship == crate::models::ContentPartAuthorship::Human
                && part.origin == crate::models::ContentPartOrigin::DirectInput
        }));
        answer.provenance.validate(&answer.content).unwrap();
    }

    #[test]
    fn aggregate_tool_results_attribute_answers_to_the_matching_block_only() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("aggregate-answer-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","sessionId":"aggregate-answer","message":{"role":"assistant","content":[{"type":"tool_use","id":"read-1","name":"Read","input":{}},{"type":"tool_use","id":"ask-1","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"aggregate-answer","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":"Your questions have been answered: \"Quality?\"=\"yes\"."},{"type":"tool_result","tool_use_id":"ask-1","content":"yes"}]},"toolUseResult":{"answers":{"Quality?":"yes"}}}"#,
                "\n",
                r#"{"type":"user","sessionId":"aggregate-answer","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"The user responded: freeform α\ncontinue"}]},"toolUseResult":{"response":"freeform α\ncontinue"}}"#,
                "\n",
                r#"{"type":"user","sessionId":"aggregate-answer","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"read-1","content":"native only α"},{"type":"tool_result","tool_use_id":"ask-1","content":"answer recorded"}]},"toolUseResult":{"response":"native only α"}}"#,
                "\n",
                r#"{"type":"user","sessionId":"aggregate-answer","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"stored elsewhere"}]},"toolUseResult":{"questions":[{"question":"z"},{"question":"a"}],"answers":{"a":"α answer","z":"z answer"}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolResult)
            .expect("aggregate tool-result row remains one source record");
        let human_parts: Vec<_> = answer
            .provenance
            .content_parts
            .iter()
            .filter(|part| part.authorship == crate::models::ContentPartAuthorship::Human)
            .collect();
        assert_eq!(human_parts.len(), 1);
        assert!(
            human_parts[0].start_char
                > "Your questions have been answered: \"Quality?\"=\"yes\"."
                    .chars()
                    .count()
        );
        let text: String = answer
            .content
            .chars()
            .skip(human_parts[0].start_char)
            .take(human_parts[0].end_char - human_parts[0].start_char)
            .collect();
        assert_eq!(text, "yes");
        answer.provenance.validate(&answer.content).unwrap();

        let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        let user_input = |query| {
            crate::message_search::MessageSearchRequest::builder(
                crate::message_search::MessageQuery::literal(query).unwrap(),
                crate::message_search::MessageTarget::content(),
            )
            .role(crate::models::Role::User)
            .build()
            .unwrap()
        };
        assert_eq!(service.search(user_input("yes")).unwrap().hits().len(), 1);
        assert_eq!(
            service
                .search(user_input("freeform α\ncontinue"))
                .unwrap()
                .hits()
                .len(),
            1
        );
        assert!(service
            .search(user_input("The user responded:"))
            .unwrap()
            .hits()
            .is_empty());
        assert_eq!(
            service
                .search(user_input("native only α"))
                .unwrap()
                .hits()
                .len(),
            1
        );
        assert!(service
            .search(user_input("answer recorded"))
            .unwrap()
            .hits()
            .is_empty());
        assert!(parsed
            .messages
            .iter()
            .any(|message| message.content == "stored elsewhere\nz answer\nα answer"));
        for query in ["z answer", "α answer"] {
            assert_eq!(service.search(user_input(query)).unwrap().hits().len(), 1);
        }
        assert!(service
            .search(user_input("z answer\nα answer"))
            .unwrap()
            .hits()
            .is_empty());
        assert!(service
            .search(user_input("Quality?"))
            .unwrap()
            .hits()
            .is_empty());
    }

    #[test]
    fn interactive_answers_keep_native_values_when_display_is_ambiguous() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("answer-safety-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","sessionId":"answer-safety","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-1","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"answer-safety","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"Your questions have been answered: \"Did you say yes?\"=\"yes\"."}]},"toolUseResult":{"answers":{"Did you say yes?":"yes"}}}"#,
                "\n",
                r#"{"type":"user","sessionId":"answer-safety","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"\"q\"=\"yes\" and \"q\"=\"yes\""}]},"toolUseResult":{"answers":{"q":"yes"}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let answers: Vec<_> = parsed
            .messages
            .iter()
            .filter(|message| message.kind == crate::models::MessageKind::ToolResult)
            .collect();
        assert_eq!(answers.len(), 2);
        assert_eq!(
            answers[0].provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        let human_answer = answers[0]
            .provenance
            .content_parts
            .iter()
            .find(|part| part.authorship == crate::models::ContentPartAuthorship::Human)
            .unwrap();
        let answer_text: String = answers[0]
            .content
            .chars()
            .skip(human_answer.start_char)
            .take(human_answer.end_char - human_answer.start_char)
            .collect();
        assert_eq!(answer_text, "yes");
        assert_eq!(
            answers[1].provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert_eq!(answers[1].content, "\"q\"=\"yes\" and \"q\"=\"yes\"\nyes");
        let native_answer = answers[1].provenance.content_parts.last().unwrap();
        assert_eq!(
            native_answer.authorship,
            crate::models::ContentPartAuthorship::Human
        );
        assert_eq!(native_answer.end_char - native_answer.start_char, 3);
        assert!(native_answer.start_char > "\"q\"=\"yes\" and \"q\"=\"yes\"".chars().count());
    }

    #[test]
    fn only_queue_enqueues_require_occurrence_history_for_tail_parsing() {
        let adapter = ClaudeAdapter::new(vec![]);
        for (operation, full_parse) in [("enqueue", true), ("popAll", false)] {
            let record = json!({"type":"queue-operation", "sessionId":"queue-tail-state",
                "operation":operation, "content":"queued prompt"})
            .to_string();
            let parsed = adapter
                .parse_reader(
                    std::io::Cursor::new(record),
                    std::path::Path::new("/tmp/queue-tail-state.jsonl"),
                )
                .unwrap();
            let metadata: serde_json::Value =
                serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
            assert_eq!(
                metadata["requires_full_parse_for_tail"]
                    .as_bool()
                    .unwrap_or(false),
                full_parse
            );
            assert!(parsed.session.parse_warning.is_none());
        }
    }

    #[test]
    fn ask_user_answers_are_human_and_legacy_queue_authors_remain_unknown() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("input-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","sessionId":"input-session","timestamp":"2026-06-16T20:58:00Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-1","name":"AskUserQuestion","input":{"questions":[]}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"input-session","timestamp":"2026-06-16T20:58:01Z","uuid":"answer-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"Your questions have been answered: \"quality\"=\"human answer α\"."}]},"toolUseResult":{"answers":{"quality":"human answer α"}}}"#,
                "\n",
                r#"{"type":"queue-operation","operation":"enqueue","sessionId":"input-session","timestamp":"2026-06-16T20:58:02Z","content":"queued correction"}"#,
                "\n",
                "{malformed\n",
                r#"{"type":"queue-operation","operation":"popAll","sessionId":"input-session","timestamp":"2026-06-16T20:58:03Z","content":"queued correction"}"#,
                "\n",
                r#"{"type":"queue-operation","operation":"enqueue","sessionId":"input-session","timestamp":"2026-06-16T20:58:04Z","content":"queued correction"}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert_eq!(
            parsed.messages.len(),
            4,
            "tool call, answer, and two submits"
        );

        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("human answer α"))
            .expect("structured answer remains searchable");
        assert_eq!(answer.role, crate::models::Role::Tool);
        assert_eq!(
            answer.provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert_eq!(
            answer.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        assert!(answer.provenance.content_parts.iter().any(|part| {
            part.authorship == crate::models::ContentPartAuthorship::Human
                && part.origin == crate::models::ContentPartOrigin::DirectInput
        }));
        answer
            .provenance
            .validate(&answer.content)
            .expect("interactive answer parts cover the wrapper exactly");

        let queued: Vec<_> = parsed
            .messages
            .iter()
            .filter(|message| message.content == "queued correction")
            .collect();
        assert_eq!(
            queued.len(),
            2,
            "repeated submits survive; popAll is an echo"
        );
        assert!(queued
            .iter()
            .all(|message| message.provenance.authorship
                == crate::models::MessageAuthorship::Unknown));
        assert!(queued.iter().all(|message| {
            message.provenance.input_delivery_state
                == Some(crate::models::InputDeliveryState::Submitted)
        }));
        assert!(queued.iter().all(|message| {
            message.provenance.input_origin == Some(crate::models::MessageInputOrigin::Queued)
        }));
        let queued_ids: Vec<_> = queued
            .iter()
            .map(|message| {
                message
                    .provenance
                    .correlation_identity
                    .as_ref()
                    .expect("queue records receive a source-local identity")
                    .id
                    .clone()
            })
            .collect();
        assert_ne!(queued_ids[0], queued_ids[1], "same text is not an identity");
        let reparsed = adapter.parse(&adapter.discover()[0]);
        let reparsed_ids: Vec<_> = reparsed
            .messages
            .iter()
            .filter(|message| message.content == "queued correction")
            .map(|message| {
                message
                    .provenance
                    .correlation_identity
                    .as_ref()
                    .unwrap()
                    .id
                    .clone()
            })
            .collect();
        assert_eq!(
            queued_ids, reparsed_ids,
            "source-local IDs survive full reparses"
        );
        let metadata = parsed.session.raw_metadata_json.as_deref().unwrap();
        assert!(metadata.contains("\"queue_operations\""));
        assert!(metadata.contains("\"popAll\":1"));
        let warning = parsed.session.parse_warning.as_deref().unwrap();
        assert!(warning.contains("skipped 1 malformed JSONL record"));
        assert!(!warning.contains("queue delivery, cancellation"));
    }

    #[test]
    fn interactive_answers_preserve_channel_and_scheduled_sender_uncertainty() {
        use crate::message_search::{
            MessageQuery, MessageSearchRequest, MessageTarget, SearchSurface,
        };
        use crate::models::{ContentPartAuthorship, MessageAuthorship};
        use serde_json::json;

        let temp = tempfile::tempdir().unwrap();
        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        for path in [
            temp.path().join("origin-answer.jsonl"),
            temp.path()
                .join("local-agent-mode-sessions/install/account/local_origin/audit.jsonl"),
        ] {
            for (origin, expected_author) in [
                (json!({"kind":"human"}), ContentPartAuthorship::Human),
                (
                    json!({"kind":"channel","server":"slack"}),
                    ContentPartAuthorship::Unknown,
                ),
                (
                    json!({"kind":"task-notification","subkind":"scheduled-trigger"}),
                    ContentPartAuthorship::Unknown,
                ),
            ] {
                for (display, prefix) in [
                    (
                        "Your questions have been answered: \"Q\"=\"answer marker\".",
                        "",
                    ),
                    ("answer marker", ""),
                    ("answer marker", "direct preamble"),
                ] {
                    let mut blocks = Vec::new();
                    if !prefix.is_empty() {
                        blocks.push(json!({"type":"text","text":prefix}));
                    }
                    blocks
                        .push(json!({"type":"tool_result","tool_use_id":"ask","content":display}));
                    let input = [
                    json!({"type":"assistant","sessionId":"origin-answer","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask","name":"AskUserQuestion","input":{}}]}}),
                    json!({"type":"user","sessionId":"origin-answer","uuid":"answer","origin":origin,"message":{"role":"user","content":blocks},"toolUseResult":{"answers":{"Q":"answer marker"}}}),
                ].into_iter().map(|value| value.to_string()).collect::<Vec<_>>().join("\n");
                    let parsed = adapter
                        .parse_reader(std::io::Cursor::new(input), &path)
                        .unwrap();
                    let answer = parsed
                        .messages
                        .iter()
                        .find(|message| {
                            message
                                .provenance
                                .correlation_identity
                                .as_ref()
                                .is_some_and(|identity| identity.id == "answer")
                        })
                        .unwrap();
                    let expected = if prefix.is_empty() {
                        display.to_string()
                    } else {
                        format!("{prefix}\n{display}")
                    };
                    assert_eq!(
                        answer.content, expected,
                        "native answer must appear exactly once: {origin}"
                    );
                    if answer.provenance.authorship == MessageAuthorship::Mixed {
                        let input_parts: Vec<_> = answer
                            .provenance
                            .content_parts
                            .iter()
                            .filter(|part| {
                                part.origin == crate::models::ContentPartOrigin::DirectInput
                            })
                            .collect();
                        assert!(!input_parts.is_empty());
                        assert!(
                            input_parts
                                .iter()
                                .all(|part| part.authorship == expected_author),
                            "{origin}: {:?}",
                            answer.provenance
                        );
                    } else {
                        assert_eq!(
                            answer.provenance.authorship,
                            if expected_author == ContentPartAuthorship::Human {
                                MessageAuthorship::Human
                            } else {
                                MessageAuthorship::Unknown
                            }
                        );
                    }
                    answer.provenance.validate(&answer.content).unwrap();
                    let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
                    db.upsert_session(&parsed, 0, 0).unwrap();
                    db.upsert_session(&parsed, 0, 0).unwrap();
                    let config = crate::config::Config::default();
                    let service =
                        crate::service::MessageService::new(&config, &db, SearchSurface::Rust);
                    for scope in [None, Some(crate::models::Role::User)] {
                        let response = service
                            .search(
                                {
                                    let builder = MessageSearchRequest::builder(
                                        MessageQuery::literal("answer marker").unwrap(),
                                        MessageTarget::content(),
                                    )
                                    .session_id(parsed.session.id.clone())
                                    .unwrap();
                                    if let Some(role) = scope {
                                        builder.role(role)
                                    } else {
                                        builder
                                    }
                                }
                                .build()
                                .unwrap(),
                            )
                            .unwrap();
                        assert_eq!(response.hits().len(), 1);
                        assert_eq!(response.hits()[0].message.content, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn native_origins_override_session_fallback_without_hiding_any_content() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("native-origins.jsonl");
        let phrase = "same native-origin phrase";
        let records = [
            format!(
                r#"{{"type":"user","uuid":"human","sessionId":"native","agent_id":"child-with-explicit-human","origin":{{"kind":"human"}},"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"channel","sessionId":"native","origin":{{"kind":"channel","server":"slack"}},"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"scheduled","sessionId":"native","origin":{{"kind":"task-notification","subkind":"scheduled-trigger"}},"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"peer","sessionId":"native","origin":{{"kind":"peer","agent_id":"worker"}},"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"synthetic","sessionId":"native","isSynthetic":true,"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"task-peer","sessionId":"native","origin":{{"kind":"task-notification","subkind":"peer-send-message"}},"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"peer-mixed","sessionId":"native","origin":{{"kind":"peer","agent_id":"worker"}},"message":{{"role":"user","content":[{{"type":"text","text":"{phrase}"}},{{"type":"tool_result","tool_use_id":"unknown","content":"generated peer output"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"malformed","sessionId":"native","origin":[],"message":{{"role":"user","content":[{{"type":"text","text":"{phrase}"}},{{"type":"tool_result","content":"generated ambiguous output"}}]}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"legacy","sessionId":"native","agent_id":"","parent_tool_use_id":null,"message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"sdk-child","session_id":"native","agent_id":"child-task","parent_tool_use_id":"spawn-call","message":{{"role":"user","content":"{phrase}"}}}}"#
            ),
            format!(
                r#"{{"type":"user","uuid":"sdk-child-parent-only","session_id":"native","parent_tool_use_id":"spawn-call","message":{{"role":"user","content":[{{"type":"text","text":"{phrase}"}},{{"type":"tool_result","tool_use_id":"unknown","content":"generated child output"}}]}}}}"#
            ),
        ];
        std::fs::write(&path, records.join("\n") + "\n").unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let message = |id: &str| {
            parsed
                .messages
                .iter()
                .find(|message| {
                    message
                        .provenance
                        .correlation_identity
                        .as_ref()
                        .is_some_and(|identity| identity.id == id)
                })
                .unwrap_or_else(|| panic!("missing native event {id}"))
        };
        assert_eq!(
            message("human").provenance.authorship,
            crate::models::MessageAuthorship::Human
        );
        assert_eq!(
            message("channel").provenance.authorship,
            crate::models::MessageAuthorship::Unknown
        );
        assert_eq!(
            message("scheduled").provenance.authorship,
            crate::models::MessageAuthorship::Unknown
        );
        for id in ["human", "channel", "scheduled"] {
            assert_eq!(
                message(id).provenance.input_origin,
                Some(crate::models::MessageInputOrigin::DirectConversation),
                "{id} is structurally an intended inbound input"
            );
        }
        assert_eq!(
            message("peer").provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        assert_eq!(
            message("synthetic").provenance.authorship,
            crate::models::MessageAuthorship::Generated
        );
        assert_eq!(
            message("task-peer").provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        assert_eq!(
            message("peer-mixed").provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert_eq!(message("peer-mixed").provenance.input_origin, None);
        assert_eq!(
            message("malformed").provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert_eq!(message("malformed").provenance.input_origin, None);
        assert!(message("malformed")
            .provenance
            .content_parts
            .iter()
            .all(|part| {
                part.authorship != crate::models::ContentPartAuthorship::Unknown
                    || part.origin == crate::models::ContentPartOrigin::Unknown
            }));
        assert_eq!(
            message("sdk-child").provenance.authorship,
            crate::models::MessageAuthorship::Agent,
            "SDK interleaved child input cannot inherit root fallback"
        );
        assert_eq!(
            message("sdk-child-parent-only").provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert!(message("sdk-child-parent-only")
            .provenance
            .content_parts
            .iter()
            .all(|part| part.authorship != crate::models::ContentPartAuthorship::Human));
        assert_eq!(
            message("legacy").provenance.authorship,
            crate::models::MessageAuthorship::Human,
            "missing native fields retain the existing session-level fallback"
        );

        let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        let request = |scope| {
            {
                let builder = crate::message_search::MessageSearchRequest::builder(
                    crate::message_search::MessageQuery::literal(phrase).unwrap(),
                    crate::message_search::MessageTarget::content(),
                );
                if let Some(role) = scope {
                    builder.role(role)
                } else {
                    builder
                }
            }
            .build()
            .unwrap()
        };
        assert_eq!(
            service
                .search(request(Some(crate::models::Role::User)))
                .unwrap()
                .hits()
                .len(),
            4,
            "human, channel, scheduled, and legacy rows are intended input"
        );
        assert_eq!(
            service.search(request(None)).unwrap().hits().len(),
            records.len(),
            "native exclusions affect UserInput only; Any keeps every exact row"
        );
    }

    #[test]
    fn explicit_non_user_queue_origin_does_not_override_legacy_queue_semantics() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("queue-native-origin.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"queue-operation","operation":"enqueue","uuid":"generated-queue","sessionId":"queue-native","origin":{"kind":"unclassified"},"timestamp":"2026-06-17T00:00:00Z","content":"generated queue text"}"#,
                "\n",
                r#"{"type":"queue-operation","operation":"enqueue","uuid":"legacy-queue","sessionId":"queue-native","timestamp":"2026-06-17T00:00:01Z","content":"legacy queue text"}"#,
                "\n",
            ),
        )
        .unwrap();
        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let generated = parsed
            .messages
            .iter()
            .find(|message| message.content == "generated queue text")
            .unwrap();
        assert_eq!(
            generated.provenance.authorship,
            crate::models::MessageAuthorship::Generated
        );
        assert_eq!(generated.provenance.input_origin, None);
        assert_eq!(
            generated.provenance.input_delivery_state,
            Some(crate::models::InputDeliveryState::Submitted)
        );
        let legacy = parsed
            .messages
            .iter()
            .find(|message| message.content == "legacy queue text")
            .unwrap();
        assert_eq!(
            legacy.provenance.authorship,
            crate::models::MessageAuthorship::Unknown
        );
        assert_eq!(
            legacy.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::Queued)
        );
        let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        let request = |query| {
            crate::message_search::MessageSearchRequest::builder(
                crate::message_search::MessageQuery::literal(query).unwrap(),
                crate::message_search::MessageTarget::content(),
            )
            .role(crate::models::Role::User)
            .build()
            .unwrap()
        };
        assert!(service
            .search(request("generated queue text"))
            .unwrap()
            .hits()
            .is_empty());
        assert_eq!(
            service
                .search(request("legacy queue text"))
                .unwrap()
                .hits()
                .len(),
            1
        );
    }

    #[test]
    fn desktop_sdk_snake_case_answers_and_native_origin_are_retained() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("local-agent-mode-sessions");
        let session_dir = root.join("install/account/local_desktop-native");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("audit.jsonl"),
            concat!(
                r#"{"type":"assistant","session_id":"desktop-native","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-desktop","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","uuid":"desktop-freeform","session_id":"desktop-native","origin":{"kind":"human"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-desktop","content":"The user responded: freeform desktop"}]},"tool_use_result":{"response":"freeform desktop"}}"#,
                "\n",
                r#"{"type":"assistant","session_id":"desktop-native","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-desktop-structured","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","uuid":"desktop-structured","session_id":"desktop-native","origin":{"kind":"human"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-desktop-structured","content":"Your questions have been answered: \"Deploy?\"=\"yes\"."}]},"tool_use_result":{"questions":[{"question":"Deploy?"}],"answers":{"Deploy?":"yes"}}}"#,
                "\n",
                r#"{"type":"user","uuid":"desktop-peer","session_id":"desktop-native","origin":{"kind":"peer","agent_id":"worker"},"message":{"role":"user","content":"desktop peer context"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("freeform desktop"))
            .expect("snake_case SDK answer remains indexed");
        assert_eq!(
            answer.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        assert!(answer.provenance.content_parts.iter().any(|part| {
            part.authorship == crate::models::ContentPartAuthorship::Human
                && part.origin == crate::models::ContentPartOrigin::DirectInput
        }));
        let structured = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("yes"))
            .expect("snake_case structured answer remains indexed");
        assert_eq!(
            structured.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        let peer = parsed
            .messages
            .iter()
            .find(|message| message.content == "desktop peer context")
            .expect("Desktop peer record remains searchable in Any");
        assert_eq!(
            peer.provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        assert_eq!(peer.provenance.input_origin, None);
    }

    #[test]
    fn queue_upgrade_repairs_archived_authorship_without_losing_submissions() {
        use crate::models::{MessageAuthorship, MessageFilters};

        for (relative, old_version, provider) in [
            ("project/input.jsonl", "claude-v5", Provider::Claude),
            (
                "local-agent-mode-sessions/install/account/local_input/audit.jsonl",
                "claude-desktop-local-agent-v5",
                Provider::ClaudeDesktop,
            ),
        ] {
            let temp = tempdir().unwrap();
            let path = temp.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let archived = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"input","timestamp":"2026-06-16T20:58:02Z","content":"/queue input"}"#;
            let retained = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"input","timestamp":"2026-06-16T20:58:03Z","content":"retained queue input"}"#;
            std::fs::write(&path, format!("{archived}\n{retained}\n")).unwrap();
            let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
            let source = adapter.discover().remove(0);
            let mut old = adapter.parse(&source);
            assert_eq!(old.session.provider, provider);
            old.session.parse_version = old_version.into();
            for message in &mut old.messages {
                message.provenance.authorship = MessageAuthorship::Human;
            }
            let db_path = temp.path().join("index.db");
            let db = crate::db::Db::open(&db_path).unwrap();
            db.upsert_session(&old, 1, 1).unwrap();
            let human = MessageFilters {
                role: Some(crate::models::Role::User),
                ..Default::default()
            };
            assert_eq!(db.search_messages("queue input", &human).unwrap().len(), 2);
            std::fs::write(&path, format!("{retained}\n")).unwrap();
            let repaired = adapter.parse(&source);
            for generation in [2, 3] {
                db.upsert_session(&repaired, generation, generation)
                    .unwrap();
                assert_eq!(
                    db.search_messages("queue input", &Default::default())
                        .unwrap()
                        .len(),
                    2
                );
                assert_eq!(db.search_messages("queue input", &human).unwrap().len(), 2);
            }
            let stored = rusqlite::Connection::open(db_path).unwrap();
            let unknown: i64 = stored.query_row("select count(*) from messages where authorship='unknown' and input_origin='queued' and input_delivery_state='submitted'", [], |row| row.get(0)).unwrap();
            assert_eq!(unknown, 2);
        }
    }

    #[test]
    fn queued_source_local_identity_survives_shortening_same_timestamp_prefix() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("queue-identity-session.jsonl");
        let first = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-identity","timestamp":"2026-06-16T20:58:02Z","content":"first queued input"}"#;
        let surviving = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-identity","timestamp":"2026-06-16T20:58:02Z","content":"surviving queued input"}"#;
        std::fs::write(&path, format!("{first}\n{surviving}\n")).unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let initial = adapter.parse(&adapter.discover()[0]);
        let initial_survivor_id = initial
            .messages
            .iter()
            .find(|message| message.content == "surviving queued input")
            .and_then(|message| message.provenance.correlation_identity.as_ref())
            .expect("surviving enqueue has source-local identity")
            .id
            .clone();

        // Removing an earlier, different enqueue must not shift the surviving message into a
        // different identity bucket. The digest is part of the bucket; the occurrence suffix is
        // reserved for genuinely repeated identical submissions.
        std::fs::write(&path, format!("{surviving}\n")).unwrap();
        let shortened = adapter.parse(&adapter.discover()[0]);
        let shortened_survivor_id = shortened
            .messages
            .iter()
            .find(|message| message.content == "surviving queued input")
            .and_then(|message| message.provenance.correlation_identity.as_ref())
            .expect("surviving enqueue remains source-locally identified")
            .id
            .clone();
        assert_eq!(initial_survivor_id, shortened_survivor_id);
    }

    #[test]
    fn shortening_identical_queued_inputs_keeps_both_archived_submissions() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("queue-repeats.jsonl");
        let record = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-repeats","timestamp":"2026-06-16T20:58:02Z","content":"repeated queued input"}"#;
        std::fs::write(&path, format!("{record}\n{record}\n")).unwrap();
        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
        let initial = adapter.parse(&adapter.discover()[0]);
        db.upsert_session(&initial, 1, 1).unwrap();
        std::fs::write(&path, format!("{record}\n")).unwrap();
        let shortened = adapter.parse(&adapter.discover()[0]);
        assert!(shortened.session.parse_warning.is_none());
        // With no provider identity these identical events cannot be distinguished after a
        // rewrite. The archive keeps both submissions without inventing a delivery outcome.
        let filters = crate::models::MessageFilters {
            role: Some(crate::models::Role::User),
            ..Default::default()
        };
        for generation in [2, 3] {
            db.upsert_session(&shortened, generation, generation)
                .unwrap();
            assert_eq!(
                db.search_messages("repeated queued input", &filters)
                    .unwrap()
                    .len(),
                2
            );
            assert_eq!(
                db.search_messages("repeated queued input", &Default::default())
                    .unwrap()
                    .len(),
                2
            );
            let stored = rusqlite::Connection::open(temp.path().join("index.db")).unwrap();
            let (identities, submitted, unknown): (i64, i64, i64) = stored.query_row(
                "select count(distinct correlation_id), sum(input_delivery_state='submitted'), sum(authorship='unknown') from messages",
                [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).unwrap();
            assert_eq!((identities, submitted, unknown), (2, 2, 2));
        }
    }

    #[test]
    #[cfg(unix)]
    fn verified_source_aliases_do_not_duplicate_identical_queue_submissions() {
        let temp = tempdir().unwrap();
        let real_root = temp.path().join("real");
        std::fs::create_dir(&real_root).unwrap();
        let alias_root = temp.path().join("alias");
        std::os::unix::fs::symlink(&real_root, &alias_root).unwrap();
        let record = r#"{"type":"queue-operation","operation":"enqueue","sessionId":"queue-alias","timestamp":"2026-06-16T20:58:02Z","content":"same queued submission"}"#;
        std::fs::write(
            real_root.join("session.jsonl"),
            format!("{record}\n{record}\n"),
        )
        .unwrap();
        let old_adapter = ClaudeAdapter::new(vec![alias_root]);
        let old_source = old_adapter.discover().remove(0);
        let initial = old_adapter.parse(&old_source);
        let new_adapter = ClaudeAdapter::new(vec![real_root]);
        let new_source = new_adapter.discover().remove(0);
        assert_eq!(
            std::fs::canonicalize(&old_source.path).unwrap(),
            std::fs::canonicalize(&new_source.path).unwrap()
        );
        let reparsed = new_adapter.parse(&new_source);
        let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
        db.upsert_session(&initial, 1, 1).unwrap();
        for generation in [2, 3] {
            db.upsert_session_reconciling_sources(
                &reparsed,
                generation,
                generation,
                std::slice::from_ref(&initial.session.source_path),
                false,
            )
            .unwrap();
            assert_eq!(
                db.message_count().unwrap(),
                2,
                "aliases are one source; its two identical submissions remain independent"
            );
        }
    }

    #[test]
    fn ask_user_answer_without_wrapper_is_whole_human() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("answer-session.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"type":"assistant","sessionId":"answer-session","message":{"role":"assistant","content":[{"type":"tool_use","id":"ask-1","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"type":"user","sessionId":"answer-session","uuid":"answer-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"ask-1","content":"human answer"}]},"toolUseResult":{"answers":{"question":"human answer"}}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content == "human answer")
            .expect("answer row");
        assert_eq!(
            answer.provenance.authorship,
            crate::models::MessageAuthorship::Human
        );
        assert_eq!(
            answer.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        assert!(answer.provenance.content_parts.is_empty());
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
            let text = format!(
                "<command-name>{cmd}</command-name><command-message>{cmd}</command-message><command-args></command-args>"
            );
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
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
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

    #[test]
    fn file_edits_commit_only_after_correlated_successful_tool_results() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let session_id = "22222222-3333-4444-5555-666666666666";
        let file = root.join(format!("{session_id}.jsonl"));
        std::fs::write(
            &file,
            r#"{"sessionId":"22222222-3333-4444-5555-666666666666","type":"user","cwd":"/tmp/proj","message":{"role":"user","content":"recover the file"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"write-ok","name":"Write","input":{"file_path":"/tmp/proj/a.rs","content":"alpha"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"write-ok","content":"written"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"edit-ok","name":"Edit","input":{"file_path":"/tmp/proj/a.rs","old_string":"alpha","new_string":"beta"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"edit-ok","is_error":false,"content":"edited"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"edit-failed","name":"Edit","input":{"file_path":"/tmp/proj/a.rs","old_string":"beta","new_string":"corrupt"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"edit-failed","is_error":true,"content":"old_string not found"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"multi-ok","name":"MultiEdit","input":{"file_path":"/tmp/proj/a.rs","edits":[{"old_string":"beta","new_string":"gamma"},{"old_string":"gamma","new_string":"delta"}]}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"multi-ok","is_error":false,"content":"edited"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"write-later","name":"Write","input":{"file_path":"/tmp/proj/a.rs","content":"snapshot"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"write-later","is_error":false,"content":"written"}]}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"edit-unresolved","name":"Edit","input":{"file_path":"/tmp/proj/a.rs","old_string":"snapshot","new_string":"not observed"}}]}}
"#,
        )
        .unwrap();

        let adapter = ClaudeAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert_eq!(
            parsed
                .file_edits
                .iter()
                .map(|edit| (edit.seq, edit.tool.as_str()))
                .collect::<Vec<_>>(),
            [(0, "Write"), (1, "Edit"), (2, "MultiEdit"), (3, "Write")]
        );
        assert_eq!(
            crate::files::reconstruct(&parsed.file_edits, 1).as_deref(),
            Some("alpha")
        );
        assert_eq!(
            crate::files::reconstruct(&parsed.file_edits, 2).as_deref(),
            Some("beta")
        );
        assert_eq!(
            crate::files::reconstruct(&parsed.file_edits, 3).as_deref(),
            Some("delta")
        );
        assert_eq!(
            crate::files::reconstruct(&parsed.file_edits, 4).as_deref(),
            Some("snapshot")
        );
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
        let root = temp.path().join("Claude").join("local-agent-mode-sessions");
        let parent = root.join("install-id").join("account-id");
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
            crate::util::provider_parse_version(Provider::ClaudeDesktop)
        );
        assert_eq!(parsed.session.message_count, Some(4));
        let metadata: serde_json::Value =
            serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            metadata["session_path"],
            crate::util::normalize_path(&session_dir.join("audit.jsonl"))
        );
        assert_eq!(
            metadata["metadata_path"],
            crate::util::normalize_path(
                &parent.join("local_aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.json")
            )
        );
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

    /// One transcript holding every record kind that carries text the person did not type
    /// beside the turns they did: stored request copies, a mid-turn wrapper, a compaction summary,
    /// a Stop-hook notice, an instructions attachment, queued prompts, and a record type the
    /// parser has never seen.
    const HARNESS_RECORDS: &str = include_str!("../../tests/fixtures/claude_harness_records.jsonl");

    fn parse_harness_records() -> crate::models::ParsedSession {
        let temp = tempdir().unwrap();
        let path = temp
            .path()
            .join("cafe0002-cafe-cafe-cafe-000000000001.jsonl");
        std::fs::write(&path, HARNESS_RECORDS).unwrap();
        ClaudeAdapter::new(vec![temp.path().to_path_buf()])
            .parse_reader(std::io::Cursor::new(HARNESS_RECORDS), &path)
            .unwrap()
    }

    /// Only records that are conversation become messages. Since 2026-10-21 Claude Code also
    /// writes `api-request-blob` records, stored copies of each API request message with a
    /// `message.role`, and reading the role from `message` indexed every copy as another user or
    /// assistant turn and another tool call. Their text reached corrections, repeats and user
    /// search as if the person had typed it again.
    #[test]
    fn only_conversation_records_become_messages() {
        use crate::models::{MessageAuthorship, MessageInputOrigin, MessageKind, Role};

        let parsed = parse_harness_records();
        let rows: Vec<(Role, MessageKind, &str)> = parsed
            .messages
            .iter()
            .map(|message| (message.role, message.kind, message.content.as_str()))
            .collect();
        let conversation: Vec<_> = rows
            .iter()
            .filter(|(_, kind, _)| *kind == MessageKind::Conversation)
            .map(|(role, _, content)| (*role, *content))
            .collect();
        assert_eq!(
            conversation,
            [
                (Role::User, "please list the files in the project"),
                (Role::Assistant, "Listing them now."),
                (Role::User, "you forgot to check the tests"),
                (Role::User, "also rename the output folder"),
                (Role::User, "you forgot to check the second directory"),
                (Role::User, "you missed the second directory"),
            ],
            "every conversation row is a turn a person or the model wrote, once: {rows:#?}"
        );
        let tool_rows = rows
            .iter()
            .filter(|(_, kind, _)| matches!(kind, MessageKind::ToolCall | MessageKind::ToolResult))
            .count();
        assert_eq!(tool_rows, 4, "two calls and two results: {rows:#?}");
        assert_eq!(parsed.file_edits.len(), 1, "{:#?}", parsed.file_edits);
        // A turn holding only a `Write` call is a tool call, not prose: the written file's text
        // is in the call row and in `file_edits`, never in an assistant conversation row.
        assert!(rows
            .iter()
            .any(|(_, kind, content)| *kind == MessageKind::ToolCall
                && content.contains("first draft")));
        assert!(!parsed.transcript_text.contains("first draft"));
        for needle in [
            "while you were working",
            "coding agent",
            "contributing guide",
            "future record",
        ] {
            assert!(
                rows.iter().all(|(_, _, content)| !content.contains(needle)),
                "{needle:?} is harness or unknown-record text: {rows:#?}"
            );
        }
        let notices: Vec<_> = rows
            .iter()
            .filter(|(_, kind, _)| *kind == MessageKind::HarnessNotice)
            .map(|(_, _, content)| *content)
            .collect();
        assert_eq!(notices.len(), 3, "{notices:#?}");
        assert!(notices[0].starts_with("<task-notification>"));
        assert!(notices[1].starts_with("Background job report"));
        assert!(notices[2].starts_with("Stop hook feedback"));
        assert_eq!(
            rows.iter()
                .filter(|(role, _, _)| *role == Role::Compaction)
                .count(),
            1
        );

        // The prompt that exists only in a queued-command attachment is the person's input,
        // delivered through the queue, at the attachment's own time.
        let queued = parsed
            .messages
            .iter()
            .find(|message| message.content == "also rename the output folder")
            .unwrap();
        assert_eq!(queued.provenance.authorship, MessageAuthorship::Human);
        assert_eq!(
            queued.provenance.input_origin,
            Some(MessageInputOrigin::Queued)
        );
        assert_eq!(
            queued.ts.map(crate::util::stored_timestamp).as_deref(),
            Some("2026-10-30T10:00:07+00:00")
        );
        assert!(parsed
            .transcript_text
            .contains("also rename the output folder"));
        assert!(!parsed.transcript_text.contains("while you were working"));

        // What the earlier parser made of the declined records, for reindex to remove.
        assert_eq!(parsed.retracted.file_edits.len(), 1);
        assert!(parsed
            .retracted
            .messages
            .iter()
            .any(|message| message.content.contains("while you were working")));
        assert!(parsed
            .retracted
            .messages
            .iter()
            .all(|message| message.ts.is_none()
                || message.content.contains("future")
                || message.content == "first draft"));
    }

    /// An index written by the parser that read request copies as turns is corrected on reindex:
    /// rows derived from records still in the source, which this parser declines, leave the
    /// index, while a row whose record was pruned from the source stays, because the index is
    /// the archive of what the source once held.
    #[test]
    fn reindex_removes_rows_of_declined_records_and_keeps_archived_rows() {
        let temp = tempdir().unwrap();
        let path = temp
            .path()
            .join("cafe0002-cafe-cafe-cafe-000000000001.jsonl");
        let pruned = r#"{"type":"user","uuid":"u-0","timestamp":"2026-10-30T09:59:00Z","sessionId":"cafe0002-cafe-cafe-cafe-000000000001","message":{"role":"user","content":"an early prompt the harness later pruned"}}"#;
        std::fs::write(&path, format!("{pruned}\n{HARNESS_RECORDS}")).unwrap();
        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        let source = adapter.discover().remove(0);

        // The index the earlier parser wrote: every declined record's rows stored as well.
        let mut earlier = adapter.parse(&source);
        let declined = std::mem::take(&mut earlier.retracted);
        // The earlier parser also wrote each declined conversation record into the session text.
        for message in declined
            .messages
            .iter()
            .filter(|message| message.kind == crate::models::MessageKind::Conversation)
        {
            earlier.transcript_text.push_str("\n\n");
            earlier
                .transcript_text
                .push_str(&crate::util::format_transcript_line(
                    message.role.as_str(),
                    message.ts,
                    &message.content,
                ));
        }
        earlier.messages.extend(declined.messages);
        for (seq, message) in earlier.messages.iter_mut().enumerate() {
            message.seq = seq as i64;
        }
        earlier.file_edits.extend(declined.file_edits);
        for (seq, edit) in earlier.file_edits.iter_mut().enumerate() {
            edit.seq = seq as i64;
        }
        earlier.session.parse_version = "claude-v9".into();
        let db_path = temp.path().join("index.db");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.upsert_session(&earlier, 1, 1).unwrap();

        // The harness prunes the early prompt; the upgraded parser reindexes, twice.
        std::fs::write(&path, HARNESS_RECORDS).unwrap();
        let current = adapter.parse(&adapter.discover().remove(0));
        let stored = rusqlite::Connection::open(&db_path).unwrap();
        let count = |sql: &str| -> i64 { stored.query_row(sql, [], |row| row.get(0)).unwrap() };
        for generation in [2, 3] {
            db.upsert_session(&current, generation, generation).unwrap();
            assert_eq!(
                count(
                    "select count(*) from messages where content like '%while you were working%'
                       or content like '%future record%' or content like '%contributing guide%'"
                ),
                0,
                "rows of declined records leave the index"
            );
            assert_eq!(
                count("select count(*) from messages where content = 'Listing them now.'"),
                1
            );
            assert_eq!(
                count("select count(*) from messages where kind in ('tool_call', 'tool_result')"),
                4
            );
            assert_eq!(count("select count(*) from file_edits"), 1);
            assert_eq!(
                count("select count(*) from messages where role = 'assistant' and content = 'first draft'"),
                0,
                "the written file's text left the assistant rows"
            );
            assert_eq!(
                count(
                    "select count(*) from messages
                       where content = 'an early prompt the harness later pruned'"
                ),
                1,
                "a row whose record left the source stays in the archive"
            );
            assert_eq!(
                count(
                    "select count(*) from messages where content = 'also rename the output folder'"
                ),
                1
            );
            assert_eq!(
                count("select count(*) from messages"),
                current.messages.len() as i64 + 1
            );
            // The session text follows the same rule: the declined records' text leaves, the
            // archived prompt's text stays.
            let transcript: String = stored
                .query_row("select transcript_text from transcripts", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert!(
                !transcript.contains("while you were working"),
                "{transcript}"
            );
            assert!(!transcript.contains("future record"), "{transcript}");
            assert!(
                transcript.contains("an early prompt the harness later pruned"),
                "{transcript}"
            );
            assert_eq!(
                transcript.matches("Listing them now.").count(),
                1,
                "{transcript}"
            );
        }
    }

    /// A record type the parser does not know yields nothing, even shaped like a user turn with
    /// a tool call, and is named in the session's parse warning so `doctor` can report it.
    #[test]
    fn an_unknown_record_type_yields_nothing_and_is_reported() {
        let record = r#"{"type":"brand-new-kind","uuid":"n-1","timestamp":"2026-11-01T00:00:00Z","sessionId":"unknown-kind","message":{"role":"user","content":[{"type":"text","text":"typed by nobody"},{"type":"tool_use","id":"toolu_new","name":"Write","input":{"file_path":"/tmp/x","content":"y"}}]}}
{"type":"brand-new-kind","sessionId":"unknown-kind"}
{"type":"api-request-blob","sessionId":"unknown-kind","message":{"role":"user","content":"known and ignored"}}
{"type":"user","uuid":"n-2","timestamp":"2026-11-01T00:00:01Z","sessionId":"unknown-kind","message":{"role":"user","content":"a real turn"}}
"#;
        let parsed = ClaudeAdapter::new(vec![])
            .parse_reader(
                std::io::Cursor::new(record),
                std::path::Path::new("/tmp/unknown-kind.jsonl"),
            )
            .unwrap();
        let contents: Vec<_> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, ["a real turn"]);
        assert!(parsed.file_edits.is_empty());
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some(r#"2 records of unrecognized type "brand-new-kind" were not indexed"#)
        );
        let metadata: serde_json::Value =
            serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            metadata[super::UNRECOGNIZED_RECORD_TYPES_KEY],
            json!({"brand-new-kind": 2})
        );
    }

    #[test]
    fn repeated_record_uuids_and_tool_ids_keep_distinct_bodies() {
        let user = |body| {
            json!({"type":"user","uuid":"u1","sessionId":"dedup",
            "message":{"role":"user","content":body}})
        };
        let call = |uuid, body| {
            json!({"type":"assistant","uuid":uuid,"sessionId":"dedup",
            "message":{"role":"assistant","content":[{"type":"tool_use","id":"t1",
                "name":"Write","input":{"file_path":"/tmp/test.txt","content":body}}]}})
        };
        let lines = [
            user("first"),
            user("first"),
            user("changed"),
            call("a1", "one"),
            call("a1", "one"),
            call("a2", "one"),
            call("a3", "two"),
        ]
        .into_iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
        let parsed = ClaudeAdapter::new(vec![])
            .parse_reader(
                std::io::Cursor::new(lines),
                std::path::Path::new("/tmp/dedup.jsonl"),
            )
            .unwrap();
        let users: Vec<_> = parsed
            .messages
            .iter()
            .filter(|m| m.role == crate::models::Role::User)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(users, ["first", "changed"]);
        assert_eq!(
            parsed
                .messages
                .iter()
                .filter(|m| m.kind == crate::models::MessageKind::ToolCall)
                .count(),
            2
        );
    }

    /// A queued prompt is indexed once whichever record holds it and in whatever order the
    /// records appear; an attachment adds a row only for a text no other record indexed, and
    /// repeated submissions of one text each keep their own row.
    #[test]
    fn queued_command_attachments_add_only_prompts_no_other_record_holds() {
        let attachment = |uuid: &str, prompt: &str| {
            let timestamp = if uuid == "q-4" {
                "2026-11-01T00:00:03Z"
            } else {
                "2026-11-01T00:00:00Z"
            };
            json!({"type":"attachment","uuid":uuid,"timestamp":timestamp,
                "sessionId":"queued","attachment":{"type":"queued_command","prompt":prompt,
                "commandMode":"prompt"}})
            .to_string()
        };
        let lines = [
            // Delivered before the user record that also holds it.
            attachment("q-1", "held by a later user record"),
            attachment("q-1", "held by a later user record"),
            json!({"type":"user","uuid":"u-1","sessionId":"queued",
                "message":{"role":"user","content":"held by a later user record"}})
            .to_string(),
            // An enqueue without `content`, as recent Claude Code writes it.
            json!({"type":"queue-operation","operation":"enqueue","sessionId":"queued",
                "timestamp":"2026-11-01T00:00:01Z"})
            .to_string(),
            attachment("q-2", "only in the attachment"),
            attachment("q-2", "only in the attachment"),
            // The same text submitted twice, enqueued once with content.
            json!({"type":"queue-operation","operation":"enqueue","sessionId":"queued",
                "timestamp":"2026-11-01T00:00:02Z","content":"submitted twice"})
            .to_string(),
            attachment("q-3", "submitted twice"),
            attachment("q-3", "submitted twice"),
            attachment("q-4", "submitted twice"),
        ]
        .join("\n");
        let parsed = ClaudeAdapter::new(vec![])
            .parse_reader(
                std::io::Cursor::new(lines),
                std::path::Path::new("/tmp/queued.jsonl"),
            )
            .unwrap();
        let contents: Vec<_> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            [
                "held by a later user record",
                "only in the attachment",
                "submitted twice",
                "submitted twice",
            ]
        );
        assert!(parsed.session.parse_warning.is_none());
        let metadata: serde_json::Value =
            serde_json::from_str(parsed.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(metadata["requires_full_parse_for_tail"], json!(true));
        let temp = tempdir().unwrap();
        let path = temp.path().join("queued.db");
        crate::db::Db::open(&path)
            .unwrap()
            .upsert_session(&parsed, 1, 1)
            .unwrap();
        let stored = rusqlite::Connection::open(&path).unwrap();
        let duplicates: i64 = stored.query_row(
            "select count(*) from (select 1 from messages where role='user' and input_origin='queued' group by session_id, content, ts having count(*)>1)",
            [], |row| row.get(0)).unwrap();
        assert_eq!(
            stored
                .query_row(
                    "select count(*) from messages where content='only in the attachment'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(duplicates, 0);
    }

    /// A `queue-operation` enqueue says only that something was queued. The `queued_command`
    /// attachment Claude Code writes when it delivers that text says who sent it
    /// (`origin.kind`) and whether it is a typed prompt or a harness notification
    /// (`commandMode`). The one row for each queued text carries that evidence; a text with no
    /// attachment keeps an unknown sender, and the message text is never used to guess one.
    #[test]
    fn queued_command_evidence_decides_the_authorship_of_the_queued_row() {
        use crate::models::{MessageAuthorship, MessageInputOrigin, MessageKind};

        let enqueue = |time: &str, content: &str| {
            json!({"type":"queue-operation","operation":"enqueue","sessionId":"q",
                "timestamp":time,"content":content})
            .to_string()
        };
        let attachment = |uuid: &str, time: &str, prompt: &str, mode: &str, origin: Value| {
            let mut attachment =
                json!({"type":"queued_command","prompt":prompt,"commandMode":mode});
            if !origin.is_null() {
                attachment["origin"] = origin;
            }
            json!({"type":"attachment","uuid":uuid,"timestamp":time,"sessionId":"q",
                "attachment":attachment})
            .to_string()
        };
        let lines = [
            enqueue("2026-11-01T00:00:01Z", "typed by the person"),
            enqueue("2026-11-01T00:00:02Z", "sender not recorded"),
            enqueue("2026-11-01T00:00:03Z", "Background task finished"),
            enqueue("2026-11-01T00:00:04Z", "attachment without origin"),
            // Older Claude Code wrote no attachment; a notification is still recognised by the
            // envelope the harness wraps it in, as for a `user` record.
            enqueue(
                "2026-11-01T00:00:05Z",
                "<task-notification><task-id>t</task-id></task-notification>",
            ),
            attachment(
                "q-1",
                "2026-11-01T00:00:11Z",
                "typed by the person",
                "prompt",
                json!({"kind":"human"}),
            ),
            attachment(
                "q-3",
                "2026-11-01T00:00:13Z",
                "Background task finished",
                "task-notification",
                Value::Null,
            ),
            attachment(
                "q-4",
                "2026-11-01T00:00:14Z",
                "attachment without origin",
                "prompt",
                Value::Null,
            ),
        ]
        .join("\n");
        let parsed = ClaudeAdapter::new(vec![])
            .parse_reader(
                std::io::Cursor::new(lines),
                std::path::Path::new("/tmp/q.jsonl"),
            )
            .unwrap();
        let rows: Vec<_> = parsed
            .messages
            .iter()
            .map(|m| {
                (
                    m.content.as_str(),
                    m.kind,
                    m.provenance.authorship,
                    m.provenance.input_origin,
                    m.ts.map(crate::util::stored_timestamp),
                )
            })
            .collect();
        let at = |second: u32| Some(format!("2026-11-01T00:00:{second:02}+00:00"));
        assert_eq!(
            rows,
            [
                (
                    "typed by the person",
                    MessageKind::Conversation,
                    MessageAuthorship::Human,
                    Some(MessageInputOrigin::Queued),
                    at(1),
                ),
                (
                    "sender not recorded",
                    MessageKind::Conversation,
                    MessageAuthorship::Unknown,
                    Some(MessageInputOrigin::Queued),
                    at(2),
                ),
                (
                    "Background task finished",
                    MessageKind::HarnessNotice,
                    MessageAuthorship::Harness,
                    None,
                    at(3),
                ),
                (
                    "attachment without origin",
                    MessageKind::Conversation,
                    MessageAuthorship::Unknown,
                    Some(MessageInputOrigin::Queued),
                    at(4),
                ),
                (
                    "<task-notification><task-id>t</task-id></task-notification>",
                    MessageKind::HarnessNotice,
                    MessageAuthorship::Harness,
                    None,
                    at(5),
                ),
            ]
        );
        assert!(!parsed.transcript_text.contains("Background task finished"));
        assert!(parsed.transcript_text.contains("typed by the person"));
    }

    /// An index written before the attachment evidence was read holds each queued row with an
    /// unknown sender. Reindexing updates those rows in place: no second copy, no lost row.
    #[test]
    fn reindex_gives_stored_queued_rows_their_attachment_evidence_in_place() {
        let temp = tempdir().unwrap();
        let path = temp
            .path()
            .join("cafe0003-cafe-cafe-cafe-000000000001.jsonl");
        let session = "cafe0003-cafe-cafe-cafe-000000000001";
        let enqueues = [
            json!({"type":"queue-operation","operation":"enqueue","sessionId":session,
                "timestamp":"2026-11-01T00:00:01Z","content":"typed by the person"}),
            json!({"type":"queue-operation","operation":"enqueue","sessionId":session,
                "timestamp":"2026-11-01T00:00:02Z","content":"Background task finished"}),
        ]
        .map(|value| value.to_string())
        .join("\n");
        let attachments = [
            json!({"type":"attachment","uuid":"q-1","timestamp":"2026-11-01T00:00:03Z",
                "sessionId":session,"attachment":{"type":"queued_command",
                "prompt":"typed by the person","commandMode":"prompt","origin":{"kind":"human"}}}),
            json!({"type":"attachment","uuid":"q-2","timestamp":"2026-11-01T00:00:04Z",
                "sessionId":session,"attachment":{"type":"queued_command",
                "prompt":"Background task finished","commandMode":"task-notification"}}),
        ]
        .map(|value| value.to_string())
        .join("\n");
        let adapter = ClaudeAdapter::new(vec![temp.path().to_path_buf()]);
        std::fs::write(&path, &enqueues).unwrap();
        let mut earlier = adapter.parse(&adapter.discover().remove(0));
        earlier.session.parse_version = "claude-v10".into();
        let db_path = temp.path().join("index.db");
        let db = crate::db::Db::open(&db_path).unwrap();
        db.upsert_session(&earlier, 1, 1).unwrap();

        std::fs::write(&path, format!("{enqueues}\n{attachments}")).unwrap();
        let current = adapter.parse(&adapter.discover().remove(0));
        db.upsert_session(&current, 2, 2).unwrap();
        let stored = rusqlite::Connection::open(&db_path).unwrap();
        let rows: Vec<(String, String, String)> = stored
            .prepare("select content, kind, authorship from messages order by seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                (
                    "typed by the person".into(),
                    "conversation".into(),
                    "human".into()
                ),
                (
                    "Background task finished".into(),
                    "harness_notice".into(),
                    "harness".into()
                ),
            ]
        );
    }
}
