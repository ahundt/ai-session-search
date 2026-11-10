// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};

use crate::hashing::sha256;
use crate::models::{
    ContentPartAuthorship, ContentPartOrigin, Message, MessageAuthorship, MessageContentPart,
    MessageCorrelationAuthority, MessageCorrelationIdentity, MessageInputOrigin, MessageKind,
    ParsedSession, Provider, Role, SourceFile,
};
use crate::providers::snapshot::{
    parsed_session_from_raw_with_evidence, source_file, SnapshotMetadata,
};
use crate::providers::{walk_roots, ProviderDiscovery};
use crate::util::{
    format_transcript_line, minimal_record, parse_datetime, RawMessage, UserRoleAuthorshipEvidence,
};

pub struct GeminiCliAdapter {
    roots: Vec<PathBuf>,
    project_paths: HashMap<String, String>,
}

impl GeminiCliAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let project_paths = roots
            .iter()
            .flat_map(|root| {
                let mut candidates = vec![root.clone()];
                if let Some(parent) = root.parent() {
                    candidates.push(parent.to_path_buf());
                }
                candidates
            })
            .flat_map(|root| known_project_paths(&root))
            .collect();
        Self {
            roots,
            project_paths,
        }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        self.discover_with_warnings().sources
    }

    pub(crate) fn discover_with_warnings(&self) -> ProviderDiscovery {
        let mut sources = Vec::new();
        // Root chats are three levels below the configured tmp root; nested subagent chats add
        // a parent-id directory. Keep the bound explicit rather than walking arbitrary project
        // trees, while accepting the flat child layout used when the parent id is unavailable.
        let walked = walk_roots(&self.roots, Some(5));
        for entry in walked.entries {
            let path = &entry.path;
            if is_chat_source(&entry.root, path) && entry.metadata.is_file() {
                sources.push(source_file(
                    Provider::GeminiCli,
                    entry.path,
                    &entry.metadata,
                ));
            }
        }
        ProviderDiscovery {
            sources,
            warnings: walked.warnings,
        }
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        self.parse_path(&source.path).unwrap_or_else(|error| {
            minimal_record(Provider::GeminiCli, &source.path, format!("{error:#}"))
        })
    }

    fn parse_path(&self, path: &Path) -> Result<ParsedSession> {
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            let file = fs::File::open(path)
                .with_context(|| format!("failed to read Gemini CLI session {}", path.display()))?;
            return self.parse_reader(std::io::BufReader::new(file), path);
        }
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read Gemini CLI session {}", path.display()))?;
        self.parse_raw(path, raw)
    }

    /// Parse Gemini's append-only JSONL recording. The active record map follows the writer's
    /// native patch/rewind operations, while `history` keeps the first evidenced input that a
    /// later mask or rewind would otherwise erase on the first index.
    pub(crate) fn parse_reader<R: BufRead>(&self, reader: R, path: &Path) -> Result<ParsedSession> {
        let mut replay = NativeReplay::default();
        let mut line_count = 0usize;
        let mut malformed_line_count = 0usize;
        let mut valid_record_count = 0usize;
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
            replay.apply(value);
        }

        let provider_id = replay
            .session_id
            .as_deref()
            .or_else(|| path.file_stem().and_then(|value| value.to_str()))
            .unwrap_or("session");
        let evidence = gemini_session_evidence(replay.kind.as_deref());
        let unknown_kind = replay
            .kind
            .as_deref()
            .filter(|kind| !matches!(*kind, "main" | "subagent"));
        let mut rendered = replay.render(evidence, provider_id);
        let history = replay.history_messages(evidence, &rendered.keys);
        rendered.messages.extend(history);
        let created_at = replay.start_time.as_deref().and_then(parse_datetime);
        let updated_at = replay.last_updated.as_deref().and_then(parse_datetime);
        let mut parsed = parsed_session_from_raw_with_evidence(
            Provider::GeminiCli,
            path,
            SnapshotMetadata {
                provider_session_id: provider_id,
                cwd: self.resolve_project_path(path),
                created_at,
                updated_at,
                discovery_source: "jsonl",
            },
            rendered.messages,
            evidence,
        );
        let mut metadata = json!({
            "line_count": line_count,
            "valid_record_count": valid_record_count,
            "session_path": crate::util::normalize_path(path),
        });
        if malformed_line_count > 0 {
            metadata["malformed_line_count"] = json!(malformed_line_count);
            parsed.session.parse_warning = super::malformed_jsonl_warning(malformed_line_count);
        }
        if let Some(kind) = unknown_kind {
            metadata["unknown_session_kind"] = json!(kind);
        }
        parsed.session.raw_metadata_json = Some(metadata.to_string());
        Ok(parsed)
    }

    pub(crate) fn parse_raw(&self, path: &Path, raw: String) -> Result<ParsedSession> {
        let data: Value = serde_json::from_str(&raw).context("invalid Gemini CLI JSON")?;
        let strip_references = referenced_file_block_regex()?;
        let evidence = gemini_session_evidence(data.get("kind").and_then(Value::as_str));
        let mut messages = Vec::new();
        for message in data
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let timestamp = message
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);
            let event_id = message
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty());
            let role = match message.get("type").and_then(Value::as_str) {
                Some("user") => "user",
                Some("gemini") => "assistant",
                // Hook and CLI notices the harness wrote into the chat: what it told the agent,
                // kept findable under `harness_notice` and out of results by default.
                Some("warning" | "info" | "error") => {
                    let content = content_text(message.get("content"));
                    if !content.trim().is_empty() {
                        messages.push(RawMessage::harness_notice(content, timestamp));
                    }
                    continue;
                }
                _ => continue,
            };
            // The API echoes each tool result back as a `functionResponse` part on a user turn;
            // the `gemini` turn's `toolCalls` already carry the same result with the call, so a
            // user turn made only of echoes adds nothing.
            if role == "user" && is_function_response_echo(message.get("content")) {
                continue;
            }
            let content = content_text(message.get("content"));
            let content = strip_references
                .replace_all(&content, "")
                .trim()
                .to_string();
            if !content.is_empty() {
                let mut raw_message = RawMessage::message(role, content, timestamp, None);
                if let Some(event_id) = event_id {
                    raw_message = raw_message
                        .with_native_event_identity(MessageCorrelationAuthority::Google, event_id);
                }
                messages.push(raw_message);
            }
            // Tool traffic lives on the `gemini` turn: one call row (name, args, id) and, when the
            // CLI recorded an outcome, one result row bound to the same id.
            for call in message
                .get("toolCalls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = call.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let call_id = call.get("id").and_then(Value::as_str);
                let call_ts = call
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(parse_datetime)
                    .or(timestamp);
                let args = call.get("args").cloned().unwrap_or(Value::Null);
                let mut call_raw = RawMessage::tool_call(name, args, call_id, call_ts);
                if let Some(call_id) = call_id {
                    call_raw = call_raw.with_native_event_identity(
                        MessageCorrelationAuthority::Google,
                        format!("tool_call:{call_id}"),
                    );
                }
                messages.push(call_raw);
                let decoded_answer = call_answer(call);
                let result = tool_result_text(call);
                if result.is_some() || decoded_answer.is_some() {
                    let mut result_raw = tool_result_with_answer(
                        name,
                        result,
                        decoded_answer
                            .as_ref()
                            .map(|(_, answers)| answers.as_slice()),
                        call_id,
                        call_ts,
                        evidence,
                    );
                    if let Some(call_id) = call_id {
                        let identity = if decoded_answer.is_some() {
                            format!("answer:{call_id}")
                        } else {
                            format!("tool_result:{call_id}")
                        };
                        result_raw = result_raw.with_native_event_identity(
                            MessageCorrelationAuthority::Google,
                            identity,
                        );
                    }
                    messages.push(result_raw);
                }
            }
        }
        let provider_id = data
            .get("sessionId")
            .and_then(Value::as_str)
            .or_else(|| path.file_stem().and_then(|value| value.to_str()))
            .unwrap_or("session");
        let created_at = data
            .get("startTime")
            .and_then(Value::as_str)
            .and_then(parse_datetime)
            .or_else(|| timestamp_from_filename(path));
        let updated_at = data
            .get("lastUpdated")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        Ok(parsed_session_from_raw_with_evidence(
            Provider::GeminiCli,
            path,
            SnapshotMetadata {
                provider_session_id: provider_id,
                cwd: self.resolve_project_path(path),
                created_at,
                updated_at,
                discovery_source: "json",
            },
            messages,
            evidence,
        ))
    }

    fn resolve_project_path(&self, path: &Path) -> Option<String> {
        self.roots.iter().find_map(|root| {
            let relative = path.strip_prefix(root).ok()?;
            relative.components().find_map(|component| {
                self.project_paths
                    .get(component.as_os_str().to_str()?)
                    .cloned()
            })
        })
    }
}

/// Read the native session identity used to prove the legacy JSON → native JSONL migration pair.
///
/// This is intentionally stricter than the normal parser: a family proof must not silently pick
/// the first ID from a malformed or conflicting file. The scan is one pass over the source and
/// returns `None` when no non-empty native ID is present. It is only used for exact sibling-family
/// detection; ordinary parsing retains its existing best-effort behaviour.
#[cfg(test)]
pub(crate) fn native_session_id(path: &Path) -> Result<Option<String>> {
    native_session_id_until(path, &|| false)
}

/// Cancellable variant used by family discovery. The callback is checked by the reader rather
/// than only between files, so a large JSON/JSONL member cannot delay an index cancellation until
/// its complete contents have been buffered.
pub(crate) fn native_session_id_until(
    path: &Path,
    should_cancel: &dyn Fn() -> bool,
) -> Result<Option<String>> {
    let extension = path.extension().and_then(|value| value.to_str());
    let mut found: Option<String> = None;
    let mut observe = |value: &Value| -> Result<()> {
        let Some(id) = value.get("sessionId").and_then(Value::as_str) else {
            return Ok(());
        };
        let id = id.trim();
        if id.is_empty() {
            return Ok(());
        }
        if let Some(previous) = &found {
            if previous != id {
                bail!(
                    "conflicting Gemini CLI sessionId values in {}: {previous:?} and {id:?}",
                    path.display()
                );
            }
        } else {
            found = Some(id.to_string());
        }
        Ok(())
    };
    match extension {
        Some("json") => {
            let file = fs::File::open(path)
                .with_context(|| format!("failed to read Gemini CLI session {}", path.display()))?;
            let mut reader = BufReader::new(CancellationReader {
                inner: file,
                should_cancel,
            });
            let value: Value = serde_json::from_reader(&mut reader)
                .with_context(|| format!("invalid Gemini CLI JSON in {}", path.display()))?;
            observe(&value)?;
        }
        Some("jsonl") => {
            let file = fs::File::open(path)
                .with_context(|| format!("failed to read Gemini CLI session {}", path.display()))?;
            let reader = BufReader::new(CancellationReader {
                inner: file,
                should_cancel,
            });
            for (line_number, line) in reader.lines().enumerate() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let value: Value = serde_json::from_str(&line).with_context(|| {
                    format!(
                        "invalid Gemini CLI JSONL record {} in {}",
                        line_number + 1,
                        path.display()
                    )
                })?;
                observe(&value)?;
            }
        }
        _ => return Ok(None),
    }
    Ok(found)
}

struct CancellationReader<'a, R> {
    inner: R,
    should_cancel: &'a dyn Fn() -> bool,
}

impl<R: Read> Read for CancellationReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if (self.should_cancel)() {
            return Err(std::io::Error::other("Gemini family proof cancelled"));
        }
        self.inner.read(buffer)
    }
}

/// Merge predecessor-only records into a proven JSONL successor.
///
/// Correlated records are joined only by the provider-native identity already assigned by the
/// parser. A repeated identity or changed overlapping record rejects the family rather than
/// guessing. Identity-less predecessor records are retained unless the successor already has an
/// exactly equal row; a conflicting identity-less slot rejects the family. This keeps old
/// searchable content without using equal prose as identity.
/// The merge is `O(M_predecessor + M_successor + B * P)` worst-case time where `B` is compared
/// content and `P` is the number of candidate input parts; it uses `O(M_successor + I + K)` peak
/// memory for identity/slot maps (`I` native identities, `K` copied slot-key text), with no DB I/O.
pub(crate) fn merge_migration_family(
    mut successor: ParsedSession,
    predecessor: ParsedSession,
) -> Result<ParsedSession> {
    anyhow::ensure!(
        successor.session.provider == Provider::GeminiCli
            && predecessor.session.provider == Provider::GeminiCli,
        "Gemini migration family contains a non-Gemini session"
    );
    anyhow::ensure!(
        !successor.session.provider_session_id.is_empty()
            && successor.session.provider_session_id == predecessor.session.provider_session_id,
        "Gemini migration family has mismatched native session IDs"
    );
    anyhow::ensure!(
        successor.session.parse_warning.is_none() && predecessor.session.parse_warning.is_none(),
        "Gemini migration family contains a parse-warning member"
    );

    let mut successor_by_identity = HashMap::<MessageCorrelationIdentity, usize>::new();
    let mut successor_by_slot = HashMap::<MessageSlotKey, VecDeque<usize>>::new();
    for (index, message) in successor.messages.iter().enumerate() {
        if let Some(identity) = &message.provenance.correlation_identity {
            if successor_by_identity
                .insert(identity.clone(), index)
                .is_some()
            {
                bail!("Gemini successor repeats native message identity {identity:?}");
            }
        } else {
            successor_by_slot
                .entry(MessageSlotKey::from(message))
                .or_default()
                .push_back(index);
        }
    }
    let mut predecessor_ids = HashSet::new();
    let mut additions = Vec::new();
    for message in predecessor.messages {
        if let Some(identity) = &message.provenance.correlation_identity {
            if !predecessor_ids.insert(identity.clone()) {
                bail!("Gemini predecessor repeats native message identity {identity:?}");
            }
            if let Some(index) = successor_by_identity.get(identity) {
                anyhow::ensure!(
                    message_payload_equal(&message, &successor.messages[*index])
                        || retained_input_overlap(&message, &successor.messages[*index]),
                    "Gemini migration family has conflicting content for native message identity {identity:?}"
                );
                continue;
            }
        } else if let Some(index) = successor_by_slot
            .get_mut(&MessageSlotKey::from(&message))
            .and_then(VecDeque::pop_front)
        {
            let candidate = &successor.messages[index];
            anyhow::ensure!(
                message_payload_equal(&message, candidate)
                    || retained_input_overlap(&message, candidate),
                "Gemini migration family has conflicting identity-less overlapping records"
            );
            continue;
        }
        additions.push(message);
    }
    let next_seq = successor
        .messages
        .iter()
        .map(|message| message.seq)
        .max()
        .unwrap_or(-1)
        + 1;
    for (offset, mut message) in additions.into_iter().enumerate() {
        message.seq = next_seq + offset as i64;
        successor.messages.push(message);
    }
    successor.session.message_count = Some(successor.messages.len() as i64);
    successor.session.created_at =
        match (successor.session.created_at, predecessor.session.created_at) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
    successor.session.updated_at =
        match (successor.session.updated_at, predecessor.session.updated_at) {
            (Some(left), Some(right)) => Some(left.max(right)),
            (left, right) => left.or(right),
        };
    successor.session.last_message_at = successor
        .messages
        .iter()
        .filter_map(|message| message.ts)
        .max();
    successor.transcript_text = successor
        .messages
        .iter()
        .filter(|message| message.kind == crate::models::MessageKind::Conversation)
        .map(|message| format_transcript_line(message.role.as_str(), message.ts, &message.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(successor)
}

fn message_payload_equal(left: &Message, right: &Message) -> bool {
    left.role == right.role
        && left.ts == right.ts
        && left.tool_name == right.tool_name
        && left.kind == right.kind
        && left.tool_call_id == right.tool_call_id
        && left.is_compaction == right.is_compaction
        && left.content == right.content
        && left.provenance == right.provenance
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MessageSlotKey {
    role: Role,
    ts: Option<DateTime<Utc>>,
    tool_name: Option<String>,
    kind: MessageKind,
    tool_call_id: Option<String>,
    is_compaction: bool,
}

impl From<&Message> for MessageSlotKey {
    fn from(message: &Message) -> Self {
        Self {
            role: message.role,
            ts: message.ts,
            tool_name: message.tool_name.clone(),
            kind: message.kind,
            tool_call_id: message.tool_call_id.clone(),
            is_compaction: message.is_compaction,
        }
    }
}

/// Slot keys copy only structural fields (including optional names/IDs), so retained memory is
/// proportional to their text, not merely the number of rows.
///
/// A native event may be rendered again with generated context appended, but only an explicit
/// input-region proof may treat that as the same archived event. Assistant/tool content never
/// receives this fallback: changed non-input rows remain an honest family rejection.
fn retained_input_overlap(predecessor: &Message, successor: &Message) -> bool {
    if predecessor.role != successor.role
        || predecessor.ts != successor.ts
        || predecessor.tool_name != successor.tool_name
        || predecessor.kind != successor.kind
        || predecessor.tool_call_id != successor.tool_call_id
        || predecessor.is_compaction != successor.is_compaction
        || !predecessor.provenance.is_whole_user_input()
    {
        return false;
    }
    if predecessor.content == successor.content {
        return true;
    }
    let Some(parts) = (!successor.provenance.content_parts.is_empty())
        .then_some(&successor.provenance.content_parts)
    else {
        return false;
    };
    crate::util::content_part_regions(&successor.content, parts).is_ok_and(|regions| {
        regions
            .into_iter()
            .any(|(part, text)| part.is_user_input() && text == predecessor.content)
    })
}

fn referenced_file_block_regex() -> Result<&'static Regex> {
    static REGEX: OnceLock<std::result::Result<Regex, String>> = OnceLock::new();
    REGEX
        .get_or_init(|| {
            Regex::new(r"(?s)--- Content from referenced files ---.*?--- End of content ---\n?")
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

fn is_chat_source(root: &Path, path: &Path) -> bool {
    let Some(relative) = path.strip_prefix(root).ok() else {
        return false;
    };
    let components = relative.components().collect::<Vec<_>>();
    let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };
    let Some(chats_index) = components
        .iter()
        .position(|value| value.as_os_str().to_str() == Some("chats"))
    else {
        return false;
    };
    let after_chats = components.len().saturating_sub(chats_index + 1);
    match path.extension().and_then(|value| value.to_str()) {
        Some("json") => after_chats == 1 && file_name.starts_with("session-"),
        Some("jsonl") => {
            // Main sessions use session-*.jsonl; a child may be nested below its complete
            // parent ID, or sit directly below chats when that ID was unavailable.
            after_chats == 1 || after_chats == 2
        }
        _ => false,
    }
}

#[derive(Default)]
struct NativeReplay {
    records: HashMap<String, Value>,
    order: Vec<String>,
    history: HashMap<String, RawMessage>,
    history_order: Vec<String>,
    first_user_text: HashMap<String, String>,
    first_answer_text: HashMap<String, Vec<String>>,
    session_id: Option<String>,
    start_time: Option<String>,
    last_updated: Option<String>,
    kind: Option<String>,
    synthetic_id: usize,
}

struct RenderedMessages {
    messages: Vec<RawMessage>,
    keys: HashSet<String>,
}

impl NativeReplay {
    fn apply(&mut self, value: Value) {
        self.apply_metadata(&value);
        if let Some(set) = value.get("$set") {
            self.apply_set(set);
            return;
        }
        if let Some(rewind) = value.get("$rewindTo").and_then(Value::as_str) {
            self.rewind(rewind);
            return;
        }
        if let Some(remove) = value.get("$remove") {
            self.remove_ids(remove);
            return;
        }
        if let Some(patch) = value.get("$patch") {
            self.apply_patch(patch);
            return;
        }
        if is_message_record(&value) {
            self.upsert(value);
        }
    }

    fn apply_metadata(&mut self, value: &Value) {
        if let Some(value) = value.get("sessionId").and_then(Value::as_str) {
            self.session_id.get_or_insert_with(|| value.to_string());
        }
        if let Some(value) = value.get("startTime").and_then(Value::as_str) {
            self.start_time.get_or_insert_with(|| value.to_string());
        }
        if let Some(value) = value.get("lastUpdated").and_then(Value::as_str) {
            self.last_updated = Some(value.to_string());
        }
        if let Some(value) = value.get("kind").and_then(Value::as_str) {
            self.kind = Some(value.to_string());
        }
    }

    fn apply_set(&mut self, value: &Value) {
        self.apply_metadata(value);
        if let Some(messages) = value.get("messages").and_then(Value::as_array) {
            self.records.clear();
            self.order.clear();
            for message in messages {
                if is_message_record(message) {
                    self.upsert(message.clone());
                }
            }
        }
    }

    fn apply_patch(&mut self, value: &Value) {
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.patch_message(id, value);
        }
        if let Some(updates) = value.get("updates").and_then(Value::as_array) {
            for update in updates {
                if let Some(id) = update.get("id").and_then(Value::as_str) {
                    self.patch_message(id, update);
                }
            }
        }
        if let Some(ids) = value.get("removeIds") {
            self.remove_ids(ids);
        }
        if let Some(order) = value.get("orderIds").and_then(Value::as_array) {
            let mut next = Vec::with_capacity(self.order.len());
            let mut seen = HashSet::new();
            for id in order.iter().filter_map(Value::as_str) {
                if self.records.contains_key(id) && seen.insert(id) {
                    next.push(id.to_string());
                }
            }
            let requested = next.iter().map(String::as_str).collect::<HashSet<_>>();
            let remaining = self
                .order
                .iter()
                .filter(|id| !requested.contains(id.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            next.extend(remaining);
            self.order = next;
        }
    }

    fn patch_message(&mut self, id: &str, patch: &Value) {
        // `observe` only updates the replay's history/first-seen maps; it never reads or mutates
        // `records`. Temporarily owning the entry avoids cloning a potentially large accumulated
        // JSON value for every small native patch, then restores the same map entry and order.
        let Some((owned_id, mut record)) = self.records.remove_entry(id) else {
            return;
        };
        if let Some(content) = patch.get("content") {
            record["content"] = content.clone();
        }
        if let Some(tool_calls) = patch.get("toolCalls") {
            merge_tool_call_patches(&mut record, tool_calls);
        }
        self.observe(&owned_id, &record);
        self.records.insert(owned_id, record);
    }

    fn rewind(&mut self, id: &str) {
        let Some(index) = self.order.iter().position(|current| current == id) else {
            return;
        };
        let removed = self.order.split_off(index);
        for removed_id in removed {
            self.records.remove(&removed_id);
        }
    }

    fn remove_ids(&mut self, value: &Value) {
        let ids = value
            .as_array()
            .map(|values| values.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .or_else(|| value.as_str().map(|id| vec![id]))
            .or_else(|| value.get("id").and_then(Value::as_str).map(|id| vec![id]))
            .unwrap_or_default();
        let ids = ids.into_iter().collect::<HashSet<_>>();
        for id in &ids {
            self.records.remove(*id);
        }
        self.order.retain(|current| !ids.contains(current.as_str()));
    }

    fn upsert(&mut self, record: Value) {
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                self.synthetic_id += 1;
                format!("__record_{}", self.synthetic_id)
            });
        if !self.records.contains_key(&id) {
            self.order.push(id.clone());
        }
        self.observe(&id, &record);
        self.records.insert(id, record);
    }

    fn observe(&mut self, id: &str, record: &Value) {
        if record.get("type").and_then(Value::as_str) == Some("user") {
            let text = direct_user_text(record.get("content"));
            if let Some(text) = text.filter(|text| !text.trim().is_empty()) {
                if !is_generated_context(&text) && !self.first_user_text.contains_key(id) {
                    self.first_user_text.insert(id.to_string(), text.clone());
                    self.retain_history(
                        format!("record:{id}"),
                        RawMessage::message("user", text, record_timestamp(record), None)
                            .with_native_event_identity(
                                MessageCorrelationAuthority::Google,
                                id.to_string(),
                            ),
                    );
                }
            }
            for (call_id, name, answers) in user_function_response_answers(record) {
                self.retain_answer(&call_id, &name, &answers, record_timestamp(record));
            }
        }
        if record.get("type").and_then(Value::as_str) == Some("gemini") {
            for call in record
                .get("toolCalls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(call_id) = call.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if let Some((name, answers)) = call_answer(call) {
                    self.retain_answer(call_id, &name, &answers, call_timestamp(call, record));
                }
            }
        }
    }

    fn retain_answer(
        &mut self,
        call_id: &str,
        name: &str,
        answers: &[String],
        ts: Option<DateTime<Utc>>,
    ) {
        if self.first_answer_text.contains_key(call_id) {
            return;
        }
        self.first_answer_text
            .insert(call_id.to_string(), answers.to_vec());
        self.retain_history(
            format!("answer:{call_id}"),
            answer_raw(
                name,
                answers,
                call_id,
                ts,
                self.kind.as_deref() == Some("subagent"),
            ),
        );
    }

    fn retain_history(&mut self, key: String, raw: RawMessage) {
        if self.history.contains_key(&key) {
            return;
        }
        self.history_order.push(key.clone());
        self.history.insert(key, raw);
    }

    fn render(&self, evidence: UserRoleAuthorshipEvidence, provider_id: &str) -> RenderedMessages {
        let mut rendered = RenderedMessages {
            messages: Vec::new(),
            keys: HashSet::new(),
        };
        for id in &self.order {
            let Some(record) = self.records.get(id) else {
                continue;
            };
            self.render_record(record, id, evidence, provider_id, &mut rendered);
        }
        rendered
    }

    fn render_record(
        &self,
        record: &Value,
        id: &str,
        evidence: UserRoleAuthorshipEvidence,
        _provider_id: &str,
        rendered: &mut RenderedMessages,
    ) {
        let timestamp = record_timestamp(record);
        match record.get("type").and_then(Value::as_str) {
            Some("user") => {
                let Some(text) =
                    direct_user_text(record.get("content")).filter(|text| !text.trim().is_empty())
                else {
                    return;
                };
                let first = self.first_user_text.get(id);
                // A context patch must not mask an earlier recorded user submission.
                if is_generated_context(&text) && first.is_none() {
                    rendered.keys.insert(format!("record:{id}"));
                    rendered
                        .messages
                        .push(RawMessage::harness_notice(text, timestamp));
                    return;
                }
                let changed = first.is_some_and(|first| first != &text);
                let raw = if let Some(first) = first.filter(|_| changed) {
                    let mut content = first.clone();
                    content.push('\n');
                    content.push_str(&text);
                    let first_end = first.chars().count();
                    let current_start = first_end + 1;
                    let content_end = content.chars().count();
                    let original_authorship =
                        if evidence == UserRoleAuthorshipEvidence::AgentDelegationEvent {
                            ContentPartAuthorship::Agent
                        } else if evidence == UserRoleAuthorshipEvidence::Unverified {
                            ContentPartAuthorship::Unknown
                        } else {
                            ContentPartAuthorship::Human
                        };
                    RawMessage::message("user", content, timestamp, None).with_content_parts(vec![
                        MessageContentPart {
                            ordinal: 0,
                            start_char: 0,
                            end_char: first_end,
                            authorship: original_authorship,
                            origin: ContentPartOrigin::DirectInput,
                        },
                        MessageContentPart {
                            ordinal: 1,
                            start_char: first_end,
                            end_char: current_start,
                            authorship: ContentPartAuthorship::Generated,
                            origin: ContentPartOrigin::HarnessContext,
                        },
                        MessageContentPart {
                            ordinal: 2,
                            start_char: current_start,
                            end_char: content_end,
                            authorship: ContentPartAuthorship::Unknown,
                            origin: ContentPartOrigin::HarnessContext,
                        },
                    ])
                } else {
                    let mut raw = RawMessage::message("user", text, timestamp, None);
                    if evidence == UserRoleAuthorshipEvidence::AgentDelegationEvent {
                        raw = raw.with_authorship(MessageAuthorship::Agent);
                    }
                    raw
                }
                .with_native_event_identity(MessageCorrelationAuthority::Google, id);
                let key = if changed {
                    rendered.keys.insert(format!("record:{id}"));
                    format!("record:{id}:current")
                } else {
                    format!("record:{id}")
                };
                rendered.keys.insert(key);
                rendered.messages.push(raw);
            }
            Some("gemini") => {
                let content = content_text(record.get("content"));
                if !content.trim().is_empty() {
                    rendered.keys.insert(format!("record:{id}"));
                    rendered.messages.push(
                        RawMessage::message(
                            "assistant",
                            referenced_file_block_regex()
                                .ok()
                                .map(|regex| regex.replace_all(&content, "").trim().to_string())
                                .unwrap_or(content),
                            timestamp,
                            None,
                        )
                        .with_native_event_identity(MessageCorrelationAuthority::Google, id),
                    );
                }
                for call in record
                    .get("toolCalls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(name) = call.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let call_id = call.get("id").and_then(Value::as_str);
                    let call_ts = call_timestamp(call, record);
                    let key = call_id
                        .map(|id| format!("tool:{id}"))
                        .unwrap_or_else(|| format!("tool:{id}:{name}"));
                    let mut call_raw = RawMessage::tool_call(
                        name,
                        call.get("args").cloned().unwrap_or(Value::Null),
                        call_id,
                        call_ts,
                    );
                    if let Some(call_id) = call_id {
                        call_raw = call_raw.with_native_event_identity(
                            MessageCorrelationAuthority::Google,
                            format!("tool_call:{call_id}"),
                        );
                    }
                    rendered.keys.insert(key);
                    rendered.messages.push(call_raw);
                    let decoded_answer = call_answer(call);
                    let current_answer = decoded_answer
                        .as_ref()
                        .map(|(_, answers)| answers.as_slice());
                    let historical_answer = call_id
                        .and_then(|id| self.first_answer_text.get(id))
                        .map(Vec::as_slice)
                        .filter(|answers| Some(*answers) != current_answer);
                    let result = tool_result_text(call);
                    let has_active_answer =
                        result.is_some() || historical_answer.is_some() || current_answer.is_some();
                    if has_active_answer {
                        let mut result_raw = if historical_answer.is_some() {
                            tool_result_with_answer_history(
                                name,
                                result.clone(),
                                historical_answer,
                                current_answer,
                                call_id,
                                call_ts,
                                evidence,
                            )
                        } else {
                            tool_result_with_answer(
                                name,
                                result,
                                current_answer,
                                call_id,
                                call_ts,
                                evidence,
                            )
                        };
                        if let Some(call_id) = call_id {
                            let identity =
                                if decoded_answer.is_some() || historical_answer.is_some() {
                                    format!("answer:{call_id}")
                                } else {
                                    format!("tool_result:{call_id}")
                                };
                            result_raw = result_raw.with_native_event_identity(
                                MessageCorrelationAuthority::Google,
                                identity,
                            );
                        }
                        rendered.messages.push(result_raw);
                    }
                    if has_active_answer {
                        if let Some(call_id) = call_id {
                            if historical_answer.is_some() {
                                // The active result row carries the retained first answer and the
                                // current replacement, so do not emit the retained history row too.
                                rendered.keys.insert(format!("answer:{call_id}"));
                            }
                            if current_answer.is_some() {
                                rendered.keys.insert(if historical_answer.is_some() {
                                    format!("answer:{call_id}:current")
                                } else {
                                    format!("answer:{call_id}")
                                });
                            }
                        }
                    }
                }
            }
            Some("info" | "warning" | "error") => {
                let content = content_text(record.get("content"));
                if !content.trim().is_empty() {
                    rendered.keys.insert(format!("record:{id}"));
                    rendered
                        .messages
                        .push(RawMessage::harness_notice(content, timestamp));
                }
            }
            _ => {}
        }
    }

    fn history_messages(
        &self,
        evidence: UserRoleAuthorshipEvidence,
        active_keys: &HashSet<String>,
    ) -> Vec<RawMessage> {
        self.history_order
            .iter()
            .filter(|key| !active_keys.contains(*key))
            .filter_map(|key| {
                let mut raw = self.history.get(key)?.clone();
                if evidence == UserRoleAuthorshipEvidence::AgentDelegationEvent
                    && key.starts_with("record:")
                {
                    raw = raw.with_authorship(MessageAuthorship::Agent);
                }
                if key.starts_with("answer:")
                    && evidence == UserRoleAuthorshipEvidence::AgentDelegationEvent
                {
                    raw = raw.with_authorship(MessageAuthorship::Agent);
                }
                Some(raw)
            })
            .collect()
    }
}

fn merge_tool_call_patches(record: &mut Value, patches: &Value) {
    let Some(patches) = patches.as_array() else {
        return;
    };
    let mut calls = record
        .get("toolCalls")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    // The writer's patch schema contains only `{id,result}`. Keep the original name/args and
    // update the matching call in place; a missing call is retained as a best-effort record.
    for patch in patches {
        let Some(id) = patch.get("id").and_then(Value::as_str) else {
            continue;
        };
        if let Some(call) = calls
            .iter_mut()
            .find(|call| call.get("id").and_then(Value::as_str) == Some(id))
        {
            if let Some(result) = patch.get("result") {
                call["result"] = result.clone();
            }
        } else {
            calls.push(patch.clone());
        }
    }
    record["toolCalls"] = Value::Array(calls);
}

fn is_message_record(value: &Value) -> bool {
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("user" | "gemini" | "info" | "warning" | "error")
    ) && value.get("id").is_some()
}

fn gemini_session_evidence(kind: Option<&str>) -> UserRoleAuthorshipEvidence {
    match kind {
        Some("subagent") => UserRoleAuthorshipEvidence::AgentDelegationEvent,
        Some("main") | None => UserRoleAuthorshipEvidence::HumanInputEvent,
        Some(_) => UserRoleAuthorshipEvidence::Unverified,
    }
}

fn record_timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime)
}

fn call_timestamp(call: &Value, record: &Value) -> Option<DateTime<Utc>> {
    call.get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime)
        .or_else(|| record_timestamp(record))
}

fn direct_user_text(content: Option<&Value>) -> Option<String> {
    let text = match content {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::Object(value)) => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    };
    (!text.is_empty()).then_some(text)
}

fn is_generated_context(text: &str) -> bool {
    let trimmed = text.trim_start();
    trimmed.starts_with("<session_context>") || trimmed.starts_with("<hook_context>")
}

fn is_ask_user_name(name: Option<&str>) -> bool {
    matches!(
        name.map(|name| name.to_ascii_lowercase().replace(['-', ' '], "_")),
        Some(name) if name == "ask_user" || name == "askuser"
    )
}

fn call_answer(call: &Value) -> Option<(String, Vec<String>)> {
    let name = call.get("name").and_then(Value::as_str)?;
    if !is_ask_user_name(Some(name)) {
        return None;
    }
    let parts = call.get("result")?.as_array()?;
    parts.iter().find_map(|part| {
        let response = part.get("functionResponse")?;
        let response_name = response.get("name").and_then(Value::as_str).unwrap_or(name);
        answer_from_output(response.get("response")?.get("output")?, response_name)
    })
}

fn user_function_response_answers(record: &Value) -> Vec<(String, String, Vec<String>)> {
    let Some(parts) = record.get("content").and_then(Value::as_array) else {
        return Vec::new();
    };
    parts
        .iter()
        .filter_map(|part| {
            let response = part.get("functionResponse")?;
            let call_id = response.get("id").and_then(Value::as_str)?;
            let name = response.get("name").and_then(Value::as_str)?;
            if !is_ask_user_name(Some(name)) {
                return None;
            }
            let (name, answers) =
                answer_from_output(response.get("response")?.get("output")?, name)?;
            Some((call_id.to_string(), name, answers))
        })
        .collect()
}

fn answer_from_output(output: &Value, name: &str) -> Option<(String, Vec<String>)> {
    let output = output.as_str()?;
    let object: Value = serde_json::from_str(output).ok()?;
    let answers = object.get("answers")?.as_object()?;
    let mut answers = answers.iter().collect::<Vec<_>>();
    // JSON object keys sort lexically; numeric question indexes must keep their source order.
    answers.sort_by_key(|(key, _)| (key.parse::<u64>().ok(), key.as_str()));
    let values = answers
        .into_iter()
        .filter_map(|(_, value)| value.as_str())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| (name.to_string(), values))
}

fn answer_raw(
    name: &str,
    answers: &[String],
    call_id: &str,
    ts: Option<DateTime<Utc>>,
    agent: bool,
) -> RawMessage {
    let mut raw = if agent {
        RawMessage::tool_result_with_name(
            Some(name.to_string()),
            answers.join("\n"),
            Some(call_id),
            ts,
        )
        .with_authorship(MessageAuthorship::Agent)
    } else {
        tool_result_with_answer_history(
            name,
            None,
            None,
            Some(answers),
            Some(call_id),
            ts,
            UserRoleAuthorshipEvidence::HumanInputEvent,
        )
    };
    raw = raw.with_input_origin(MessageInputOrigin::InteractiveAnswer);
    raw.with_native_event_identity(
        MessageCorrelationAuthority::Google,
        format!("answer:{call_id}"),
    )
}

fn nonempty_answers(answers: Option<&[String]>) -> Option<&[String]> {
    answers.filter(|answers| answers.iter().any(|answer| !answer.is_empty()))
}

fn append_answer_parts(
    content: &mut String,
    parts: &mut Vec<MessageContentPart>,
    answers: Option<&[String]>,
    authorship: ContentPartAuthorship,
    origin: ContentPartOrigin,
) {
    let Some(answers) = answers else {
        return;
    };
    let mut first = true;
    for answer in answers.iter().filter(|answer| !answer.is_empty()) {
        let already_separated = first
            && parts.last().is_some_and(|part| {
                part.authorship == ContentPartAuthorship::Generated
                    && part.end_char == content.chars().count()
                    && content.ends_with('\n')
            });
        if !content.is_empty() && !already_separated {
            let separator_start = content.chars().count();
            content.push('\n');
            parts.push(MessageContentPart {
                ordinal: parts.len() as u32,
                start_char: separator_start,
                end_char: separator_start + 1,
                authorship: ContentPartAuthorship::Generated,
                origin: ContentPartOrigin::ToolPayload,
            });
        }
        let answer_start = content.chars().count();
        content.push_str(answer);
        parts.push(MessageContentPart {
            ordinal: parts.len() as u32,
            start_char: answer_start,
            end_char: content.chars().count(),
            authorship,
            origin,
        });
        first = false;
    }
}

fn tool_result_with_answer(
    name: &str,
    result: Option<String>,
    answers: Option<&[String]>,
    call_id: Option<&str>,
    ts: Option<DateTime<Utc>>,
    evidence: UserRoleAuthorshipEvidence,
) -> RawMessage {
    tool_result_with_answer_history(name, result, None, answers, call_id, ts, evidence)
}

fn tool_result_with_answer_history(
    name: &str,
    result: Option<String>,
    historical_answers: Option<&[String]>,
    current_answers: Option<&[String]>,
    call_id: Option<&str>,
    ts: Option<DateTime<Utc>>,
    evidence: UserRoleAuthorshipEvidence,
) -> RawMessage {
    let historical_answers = nonempty_answers(historical_answers);
    let current_answers = nonempty_answers(current_answers);
    if historical_answers.is_none() && current_answers.is_none() {
        return result.map_or_else(
            || RawMessage::tool_result(name, String::new(), call_id, ts),
            |result| RawMessage::tool_result(name, result, call_id, ts),
        );
    }
    let answer_authorship = if evidence == UserRoleAuthorshipEvidence::AgentDelegationEvent {
        ContentPartAuthorship::Agent
    } else {
        ContentPartAuthorship::Unknown
    };
    let mut content = result.unwrap_or_default();
    let mut parts = Vec::new();
    if !content.is_empty() {
        let generated_end = content.chars().count();
        content.push('\n');
        parts.push(MessageContentPart {
            ordinal: 0,
            start_char: 0,
            end_char: generated_end + 1,
            authorship: ContentPartAuthorship::Generated,
            origin: ContentPartOrigin::ToolPayload,
        });
    }
    append_answer_parts(
        &mut content,
        &mut parts,
        historical_answers,
        answer_authorship,
        ContentPartOrigin::DirectInput,
    );
    append_answer_parts(
        &mut content,
        &mut parts,
        current_answers,
        answer_authorship,
        if historical_answers.is_some() {
            ContentPartOrigin::HarnessContext
        } else {
            ContentPartOrigin::DirectInput
        },
    );
    if parts.len() == 1 {
        let authorship = if evidence == UserRoleAuthorshipEvidence::AgentDelegationEvent {
            MessageAuthorship::Agent
        } else {
            MessageAuthorship::Unknown
        };
        return RawMessage::tool_result(name, content, call_id, ts)
            .with_authorship(authorship)
            .with_input_origin(MessageInputOrigin::InteractiveAnswer);
    }
    RawMessage::tool_result(name, content, call_id, ts)
        .with_content_parts(parts)
        .with_input_origin(MessageInputOrigin::InteractiveAnswer)
}

/// A user turn whose parts are all `functionResponse` echoes of tool results the `gemini` turn's
/// `toolCalls` already carry.
fn is_function_response_echo(content: Option<&Value>) -> bool {
    match content {
        Some(Value::Array(parts)) => {
            !parts.is_empty()
                && parts
                    .iter()
                    .all(|part| part.get("functionResponse").is_some())
        }
        _ => false,
    }
}

/// The recorded outcome of one `toolCalls` entry: the CLI's rendered `resultDisplay` when it is
/// text, else the `output` of the first `functionResponse` in `result`. `None` when the call has
/// no recorded outcome (a cancelled or still-running call).
fn tool_result_text(call: &Value) -> Option<String> {
    if let Some(display) = call.get("resultDisplay").and_then(Value::as_str) {
        let display = display.trim();
        if !display.is_empty() {
            return Some(display.to_string());
        }
    }
    let output = call
        .get("result")?
        .as_array()?
        .iter()
        .find_map(|part| part.get("functionResponse"))?
        .get("response")?
        .get("output")?;
    match output {
        Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| part.get("text").and_then(Value::as_str).unwrap_or(""))
            .collect::<Vec<_>>()
            .join(" "),
        Some(Value::Object(value)) => value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn timestamp_from_filename(path: &Path) -> Option<chrono::DateTime<chrono::Utc>> {
    let name = path.file_name()?.to_str()?;
    static REGEX: OnceLock<std::result::Result<Regex, regex::Error>> = OnceLock::new();
    let regex = REGEX
        .get_or_init(|| {
            Regex::new(r"session-(\d{4}-\d{2}-\d{2})(?:T(\d{2})(?:-(\d{2})(?:-(\d{2}))?)?)?")
        })
        .as_ref()
        .ok()?;
    let captures = regex.captures(name)?;
    parse_datetime(&format!(
        "{}T{}:{}:{}Z",
        captures.get(1)?.as_str(),
        captures.get(2).map_or("00", |value| value.as_str()),
        captures.get(3).map_or("00", |value| value.as_str()),
        captures.get(4).map_or("00", |value| value.as_str()),
    ))
}

fn known_project_paths(gemini_dir: &Path) -> HashMap<String, String> {
    let mut paths = BTreeSet::new();
    if let Some(Value::Object(values)) = read_json(gemini_dir.join("trustedFolders.json")) {
        paths.extend(values.keys().cloned());
    }
    if let Some(Value::Object(root)) = read_json(gemini_dir.join("projects.json")) {
        if let Some(Value::Object(values)) = root.get("projects") {
            paths.extend(values.keys().cloned());
        }
    }
    paths
        .into_iter()
        .map(|path| (sha256(path.as_bytes()), path))
        .collect()
}

fn read_json(path: PathBuf) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answer_output_preserves_numeric_question_order() {
        let answers: serde_json::Map<String, Value> = (0..12)
            .map(|i| (i.to_string(), Value::String(format!("answer {i}"))))
            .collect();
        let output = Value::String(json!({"answers":answers}).to_string());
        let (_, actual) = answer_from_output(&output, "ask_user").unwrap();
        assert_eq!(
            actual,
            (0..12).map(|i| format!("answer {i}")).collect::<Vec<_>>()
        );
    }

    #[test]
    fn discovers_messages_resolves_project_and_strips_file_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let gemini = dir.path().join(".gemini");
        let project = "/tmp/example-project";
        let hash = sha256(project.as_bytes());
        let chats = gemini.join("tmp").join(&hash).join("chats");
        fs::create_dir_all(&chats).unwrap();
        fs::write(
            gemini.join("trustedFolders.json"),
            format!(r#"{{"{project}":true}}"#),
        )
        .unwrap();
        let path = chats.join("session-2026-02-23T04-07-id.json");
        fs::write(
            &path,
            r#"{"sessionId":"g1","messages":[{"id":"u1","type":"user","content":"--- Content from referenced files ---secret--- End of content ---\nhello","timestamp":"2026-02-23T04:07:01Z"},{"id":"a1","type":"gemini","content":[{"text":"hi"}]}]}"#,
        )
        .unwrap();
        let adapter = GeminiCliAdapter::new(vec![gemini.join("tmp")]);

        let sources = adapter.discover();
        let parsed = adapter.parse(&sources[0]);

        assert_eq!(sources.len(), 1);
        assert_eq!(parsed.session.cwd.as_deref(), Some(project));
        assert!(parsed.session.created_at.is_some());
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].content, "hello");
        assert_eq!(parsed.messages[1].content, "hi");
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Human,
            "a Gemini CLI chat is one person-started session per file; its user turns are the \
             person's prompts once referenced-file blocks are stripped and function responses \
             are set aside"
        );
        assert_eq!(
            parsed.messages[1].provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        for (message, expected_id) in parsed.messages.iter().zip(["u1", "a1"]) {
            let identity = message
                .provenance
                .correlation_identity
                .as_ref()
                .expect("native Gemini CLI message id is retained");
            assert_eq!(
                identity.authority,
                crate::models::MessageCorrelationAuthority::Google
            );
            assert_eq!(identity.scope, "gemini-cli:g1");
            assert_eq!(identity.id, expected_id);
        }
    }

    /// The chat file records tool traffic on the `gemini` turn (`toolCalls` with args and
    /// result), echoes results back as `functionResponse` parts on a `user` turn, and writes
    /// hook and CLI notices as `warning`/`info`/`error` turns. Every one of those was skipped or
    /// mislabelled: 3,592 tool calls across 200 local files were absent from the index, and the
    /// echoed responses were user-role conversation rows of unknown authorship.
    #[test]
    fn records_tool_calls_results_notices_and_human_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-2026-02-23T04-07-id.json");
        fs::write(
            &path,
            r#"{"sessionId":"g2","messages":[
              {"id":"u1","type":"user","content":"read the notes","timestamp":"2026-02-23T04:07:01Z"},
              {"id":"a1","type":"gemini","content":[{"text":"reading"}],"timestamp":"2026-02-23T04:07:02Z",
               "toolCalls":[{"id":"read_file-1","name":"read_file","args":{"path":"notes.md"},
                 "result":[{"functionResponse":{"id":"read_file-1","name":"read_file","response":{"output":"line one"}}}],
                 "status":"success","timestamp":"2026-02-23T04:07:03Z","resultDisplay":"line one"}]},
              {"id":"u2","type":"user","content":[{"functionResponse":{"id":"read_file-1","name":"read_file","response":{"output":"line one"}}}],"timestamp":"2026-02-23T04:07:03Z"},
              {"id":"w1","type":"warning","content":"Hook(s) [cleanup] failed for event SessionEnd.","timestamp":"2026-02-23T04:07:04Z"},
              {"id":"a2","type":"gemini","content":[{"text":"done"}],"timestamp":"2026-02-23T04:07:05Z"}
            ]}"#,
        )
        .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let source = source_file(Provider::GeminiCli, path, &metadata);
        let parsed = GeminiCliAdapter::new(Vec::new()).parse(&source);

        use crate::models::{MessageAuthorship, MessageKind, Role};
        let rows: Vec<(Role, MessageKind, MessageAuthorship, Option<&str>)> = parsed
            .messages
            .iter()
            .map(|m| {
                (
                    m.role,
                    m.kind,
                    m.provenance.authorship,
                    m.tool_name.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    Role::User,
                    MessageKind::Conversation,
                    MessageAuthorship::Human,
                    None
                ),
                (
                    Role::Assistant,
                    MessageKind::Conversation,
                    MessageAuthorship::Agent,
                    None
                ),
                (
                    Role::Tool,
                    MessageKind::ToolCall,
                    MessageAuthorship::Agent,
                    Some("read_file")
                ),
                (
                    Role::Tool,
                    MessageKind::ToolResult,
                    MessageAuthorship::Generated,
                    Some("read_file")
                ),
                (
                    Role::User,
                    MessageKind::HarnessNotice,
                    MessageAuthorship::Harness,
                    None
                ),
                (
                    Role::Assistant,
                    MessageKind::Conversation,
                    MessageAuthorship::Agent,
                    None
                ),
            ],
            "{parsed:?}"
        );
        assert_eq!(
            parsed.messages[2].tool_call_id.as_deref(),
            Some("read_file-1")
        );
        assert_eq!(
            parsed.messages[3].tool_call_id.as_deref(),
            Some("read_file-1")
        );
        assert!(parsed.messages[2].content.contains("notes.md"));
        assert_eq!(parsed.messages[3].content, "line one");
        assert!(
            !parsed.transcript_text.contains("notes.md"),
            "tool traffic stays out of the session transcript, as for every other provider"
        );
        assert!(parsed.transcript_text.contains("read the notes"));
    }

    #[test]
    fn legacy_json_preserves_tool_identities_and_structured_answers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-legacy.json");
        fs::write(
            &path,
            r#"{"sessionId":"legacy","kind":"main","messages":[
              {"id":"m1","type":"gemini","content":[],"toolCalls":[{"id":"ask-1","name":"ask_user","args":{},"result":[{"functionResponse":{"id":"ask-1","name":"ask_user","response":{"output":"{\"answers\":{\"0\":\"legacy answer\"}}"}}}]}]}
            ]}"#,
        )
        .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let source = source_file(Provider::GeminiCli, path, &metadata);
        let parsed = GeminiCliAdapter::new(Vec::new()).parse(&source);
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("legacy answer"))
            .expect("legacy ask_user output is decoded");
        assert_eq!(
            answer.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        let call = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolCall)
            .expect("legacy tool call");
        assert_eq!(
            call.provenance
                .correlation_identity
                .as_ref()
                .map(|identity| identity.id.as_str()),
            Some("tool_call:ask-1")
        );
    }

    #[test]
    fn malformed_json_returns_minimal_warning_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-bad.json");
        fs::write(&path, "{").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let source = source_file(Provider::GeminiCli, path, &metadata);

        let parsed = GeminiCliAdapter::new(Vec::new()).parse(&source);

        let warning = parsed.session.parse_warning.as_deref().unwrap();
        assert!(warning.contains("invalid Gemini CLI JSON"), "{warning}");
        assert!(warning.contains("line 1 column 1"), "{warning}");
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn derives_timestamp_from_supported_filename_precisions() {
        let cases = [
            ("session-2026-02-23-id.json", "2026-02-23T00:00:00Z"),
            ("session-2026-02-23T04-07-id.json", "2026-02-23T04:07:00Z"),
            (
                "session-2026-02-23T04-07-09-id.json",
                "2026-02-23T04:07:09Z",
            ),
        ];

        for (filename, expected) in cases {
            assert_eq!(
                crate::util::display_timestamp(
                    timestamp_from_filename(Path::new(filename)).unwrap()
                ),
                expected
            );
        }
    }

    #[test]
    fn context_patch_preserves_original_user_input_on_first_index_and_refresh() {
        use crate::message_search::{
            MessageQuery, MessageSearchRequest, MessageTarget, SearchSurface,
        };
        use crate::service::MessageService;

        for preindexed in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
            let adapter = GeminiCliAdapter::new(vec![]);
            let path = Path::new("/tmp/session-context-patch.jsonl");
            let mut records = concat!(
                "{\"sessionId\":\"context-patch\",\"kind\":\"main\"}\n",
                "{\"id\":\"u1\",\"type\":\"user\",\"content\":\"original request α\"}\n",
            )
            .to_string();
            if preindexed {
                let parsed = adapter
                    .parse_reader(std::io::Cursor::new(&records), path)
                    .unwrap();
                db.upsert_session(&parsed, 1, 1).unwrap();
            }
            records.push_str("{\"$patch\":{\"id\":\"u1\",\"content\":\"<session_context>generated replacement</session_context>\"}}\n");
            for stamp in [2, 3] {
                let parsed = adapter
                    .parse_reader(std::io::Cursor::new(&records), path)
                    .unwrap();
                assert_eq!(
                    parsed
                        .messages
                        .iter()
                        .filter(|message| message.content.contains("original request α"))
                        .count(),
                    1
                );
                db.upsert_session(&parsed, stamp, stamp).unwrap();
                let config = crate::config::Config::default();
                let service = MessageService::new(&config, &db, SearchSurface::Rust);
                for (query, expected) in [("original request α", 1), ("generated replacement", 0)]
                {
                    let request = MessageSearchRequest::builder(
                        MessageQuery::literal(query).unwrap(),
                        MessageTarget::content(),
                    )
                    .role(crate::models::Role::User)
                    .build()
                    .unwrap();
                    assert_eq!(service.search(request).unwrap().hits().len(), expected);
                }
                assert_eq!(
                    db.search_messages(
                        "generated replacement",
                        &crate::models::MessageFilters::default()
                    )
                    .unwrap()
                    .len(),
                    1
                );
                assert_eq!(
                    db.message_count().unwrap(),
                    1,
                    "one native event survives context patch and refresh"
                );
            }
        }
    }

    #[test]
    fn replay_refresh_does_not_multiply_original_input_after_patch_then_remove() {
        let mut records = concat!(
            "{\"sessionId\":\"retained-input\",\"kind\":\"main\"}\n",
            "{\"id\":\"u1\",\"type\":\"user\",\"content\":\"original input α\"}\n",
            "{\"$patch\":{\"id\":\"u1\",\"content\":\"generated replacement\"}}\n",
        )
        .to_owned();
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        let adapter = GeminiCliAdapter::new(vec![]);
        let path = Path::new("/tmp/session-retained-input.jsonl");
        let filters = crate::models::MessageFilters {
            role: Some(crate::models::Role::User),
            ..Default::default()
        };
        for stamp in [1, 2, 3] {
            let parsed = adapter
                .parse_reader(std::io::Cursor::new(&records), path)
                .unwrap();
            db.upsert_session(&parsed, stamp, stamp).unwrap();
            assert_eq!(
                db.search_messages("original input α", &filters)
                    .unwrap()
                    .len(),
                1,
                "one native input survives refresh {stamp}"
            );
            assert!(db
                .search_messages("generated replacement", &filters)
                .unwrap()
                .is_empty());
            if stamp == 1 {
                records.push_str("{\"$remove\":[\"u1\"]}\n");
            }
        }
    }

    #[test]
    fn interactive_answers_keep_native_boundaries_in_service_search() {
        let records = format!(
            "{}\n{}\n",
            json!({"sessionId": "answer-boundaries", "kind": "main"}),
            json!({
                "id": "m1",
                "type": "gemini",
                "content": [],
                "toolCalls": [{
                    "id": "ask-1",
                    "name": "ask_user",
                    "args": {},
                    "result": [{
                        "functionResponse": {
                            "id": "ask-1",
                            "name": "ask_user",
                            "response": {
                                "output": json!({
                                    "answers": {"0": "one-é", "1": "two"}
                                }).to_string()
                            }
                        }
                    }]
                }]
            })
        );
        let parsed = GeminiCliAdapter::new(Vec::new())
            .parse_reader(
                std::io::Cursor::new(records),
                Path::new("session-answer-boundaries.jsonl"),
            )
            .unwrap();
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("one-é"))
            .expect("decoded interactive answers");
        assert!(answer.content.contains("one-é\ntwo"));
        assert!(answer.provenance.validate(&answer.content).is_ok());

        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        for (query, expected) in [("one-é", 1), ("two", 1), ("one-é\ntwo", 0)] {
            let request = crate::message_search::MessageSearchRequest::builder(
                crate::message_search::MessageQuery::literal(query).unwrap(),
                crate::message_search::MessageTarget::content(),
            )
            .role(crate::models::Role::User)
            .build()
            .unwrap();
            assert_eq!(
                service.search(request).unwrap().hits().len(),
                expected,
                "{query}"
            );
        }
    }

    #[test]
    fn patched_answer_retains_one_native_input_across_refresh_and_removal() {
        let answer_record = |answer: &str| {
            json!({
                "id": "m1",
                "type": "gemini",
                "content": [],
                "toolCalls": [{
                    "id": "ask-1",
                    "name": "ask_user",
                    "args": {},
                    "result": [{
                        "functionResponse": {
                            "id": "ask-1",
                            "name": "ask_user",
                            "response": {
                                "output": json!({"answers": {"0": answer}}).to_string()
                            }
                        }
                    }]
                }]
            })
        };
        let mut records = format!(
            "{}\n{}\n{}\n",
            json!({"sessionId": "patched-answer", "kind": "main"}),
            answer_record("first answer"),
            json!({"$patch": {"updates": [{
                "id": "m1",
                "toolCalls": [{
                    "id": "ask-1",
                    "result": [{"functionResponse": {
                        "id": "ask-1",
                        "name": "ask_user",
                        "response": {
                            "output": json!({"answers": {"0": "second answer"}}).to_string()
                        }
                    }}]
                }]
            }]}})
        );
        let dir = tempfile::tempdir().unwrap();
        let db = crate::db::Db::open(&dir.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        let adapter = GeminiCliAdapter::new(Vec::new());
        let path = Path::new("session-patched-answer.jsonl");
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        let filters = crate::models::MessageFilters {
            role: Some(crate::models::Role::User),
            ..Default::default()
        };
        let mut first_identity = None;
        for stamp in [1, 2, 3] {
            let parsed = adapter
                .parse_reader(std::io::Cursor::new(&records), path)
                .unwrap();
            db.upsert_session(&parsed, stamp, stamp).unwrap();
            let first = service.search_legacy("first answer", &filters).unwrap();
            assert_eq!(
                first.len(),
                1,
                "retained answer at refresh {stamp}: {:?}",
                first
                    .iter()
                    .map(|hit| (hit.seq, hit.content.clone(), hit.provenance.clone()))
                    .collect::<Vec<_>>()
            );
            assert!(service
                .search_legacy("second answer", &filters)
                .unwrap()
                .is_empty());
            let identity = (first[0].session_id.clone(), first[0].seq);
            if let Some(previous) = &first_identity {
                assert_eq!(
                    &identity, previous,
                    "native answer row identity at refresh {stamp}"
                );
            } else {
                first_identity = Some(identity);
            }
            if stamp == 1 {
                records.push_str(&format!("{}\n", json!({"$remove": ["m1"]})));
            }
        }
    }

    #[test]
    fn parses_native_jsonl_inputs_answers_and_rewind_history() {
        let records = r#"
{"sessionId":"native-1","projectHash":"project","startTime":"2026-02-23T04:07:00Z","kind":"main"}
{"id":"u1","type":"user","content":"original prompt 🦀","timestamp":"2026-02-23T04:07:01Z"}
{"id":"ctx","type":"user","content":"<session_context>generated context</session_context>","timestamp":"2026-02-23T04:07:01Z"}
{"id":"m1","type":"gemini","content":[{"text":"generated echo original prompt"}],"timestamp":"2026-02-23T04:07:02Z","toolCalls":[{"id":"ask-1","name":"ask_user","args":{},"result":[{"functionResponse":{"id":"ask-1","name":"ask_user","response":{"output":"{\"answers\":{\"0\":\"selected 🦀\"}}"}}}],"timestamp":"2026-02-23T04:07:03Z"}]}
{"id":"u2","type":"user","content":[{"functionResponse":{"id":"ask-1","name":"ask_user","response":{"output":"{\"answers\":{\"0\":\"selected 🦀\"}}"}}}],"timestamp":"2026-02-23T04:07:03Z"}
{"$rewindTo":"m1"}
not json
"#; // the last line is malformed and must not erase the valid history
        let path = Path::new("session-2026-02-23T04-07-native.jsonl");
        let parsed = GeminiCliAdapter::new(Vec::new())
            .parse_reader(std::io::Cursor::new(records), path)
            .unwrap();

        assert_eq!(parsed.session.provider_session_id, "native-1");
        assert_eq!(parsed.session.discovery_source, "jsonl");
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
        assert!(parsed
            .messages
            .iter()
            .any(|message| message.content == "original prompt 🦀"));
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content == "selected 🦀")
            .expect("rewound ask_user answer is retained");
        assert_eq!(
            answer.provenance.authorship,
            crate::models::MessageAuthorship::Unknown
        );
        assert_eq!(
            answer.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
        assert!(parsed
            .messages
            .iter()
            .any(|message| message.kind == crate::models::MessageKind::HarnessNotice));
        assert!(!parsed
            .messages
            .iter()
            .any(|message| message.content == "generated echo original prompt"));
        assert!(!parsed
            .messages
            .iter()
            .any(|message| message.content.contains("functionResponse")));
    }

    #[test]
    fn native_subagent_user_and_answer_are_not_user_input() {
        let records = r#"
{"sessionId":"child-1","kind":"subagent","startTime":"2026-02-23T04:07:00Z"}
{"id":"u1","type":"user","content":"delegated prompt"}
{"id":"m1","type":"gemini","content":[],"toolCalls":[{"id":"ask-1","name":"ask_user","args":{},"result":[{"functionResponse":{"id":"ask-1","name":"ask_user","response":{"output":"{\"answers\":{\"0\":\"delegated answer\"}}"}}}]}]}
"#;
        let parsed = GeminiCliAdapter::new(Vec::new())
            .parse_reader(std::io::Cursor::new(records), Path::new("child-1.jsonl"))
            .unwrap();
        let prompt = parsed
            .messages
            .iter()
            .find(|message| message.content == "delegated prompt")
            .expect("delegated prompt");
        assert_eq!(
            prompt.provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("delegated answer"))
            .expect("delegated answer");
        assert!(answer
            .provenance
            .content_parts
            .iter()
            .any(|part| { part.authorship == crate::models::ContentPartAuthorship::Agent }));
    }

    #[test]
    fn applies_tool_result_patch_without_losing_call_metadata() {
        let records = r#"
{"sessionId":"native-patch","kind":"main"}
{"id":"m1","type":"gemini","content":[],"toolCalls":[{"id":"ask-1","name":"ask_user","args":{}}]}
{"$patch":{"updates":[{"id":"m1","toolCalls":[{"id":"ask-1","result":[{"functionResponse":{"id":"ask-1","name":"ask_user","response":{"output":"{\"answers\":{\"0\":\"patched answer\"}}"}}}]}]}]}}
"#;
        let parsed = GeminiCliAdapter::new(Vec::new())
            .parse_reader(
                std::io::Cursor::new(records),
                Path::new("session-native-patch.jsonl"),
            )
            .unwrap();
        let answer = parsed
            .messages
            .iter()
            .find(|message| message.content.contains("patched answer"))
            .expect("patched ask_user result is decoded");
        assert_eq!(answer.tool_name.as_deref(), Some("ask_user"));
        assert_eq!(
            answer.provenance.input_origin,
            Some(crate::models::MessageInputOrigin::InteractiveAnswer)
        );
    }

    #[test]
    fn discovers_native_root_and_nested_child_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".gemini").join("tmp");
        let chats = root.join("project-hash").join("chats");
        let child_dir = chats.join("parent-session");
        fs::create_dir_all(&child_dir).unwrap();
        let main = chats.join("session-2026-02-23T04-07-main.jsonl");
        let child = child_dir.join("child-session.jsonl");
        let flat = chats.join("flat-child.jsonl");
        fs::write(&main, "{}\n").unwrap();
        fs::write(&child, "{}\n").unwrap();
        fs::write(&flat, "{}\n").unwrap();
        let sources = GeminiCliAdapter::new(vec![root]).discover();
        let mut names = sources
            .iter()
            .filter_map(|source| source.path.file_name().and_then(|name| name.to_str()))
            .collect::<Vec<_>>();
        names.sort_unstable();
        assert_eq!(
            names,
            vec![
                "child-session.jsonl",
                "flat-child.jsonl",
                "session-2026-02-23T04-07-main.jsonl"
            ]
        );
    }

    #[test]
    fn migration_identity_scan_rejects_conflicting_or_malformed_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session-identity.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"sessionId":"same","kind":"main"}"#,
                "\n",
                r#"{"sessionId":"same","id":"u1","type":"user","content":"input"}"#,
                "\n"
            ),
        )
        .unwrap();
        assert_eq!(native_session_id(&path).unwrap().as_deref(), Some("same"));

        fs::write(
            &path,
            concat!(
                r#"{"sessionId":"first"}"#,
                "\n",
                r#"{"sessionId":"second"}"#,
                "\n"
            ),
        )
        .unwrap();
        let conflict = native_session_id(&path).unwrap_err().to_string();
        assert!(conflict.contains("conflicting"), "{conflict}");

        fs::write(&path, b"not json\n").unwrap();
        let malformed = native_session_id(&path).unwrap_err().to_string();
        assert!(
            malformed.contains("invalid Gemini CLI JSONL record"),
            "{malformed}"
        );
    }

    #[test]
    fn migration_merge_keeps_predecessor_only_native_event_and_rejects_overlap_change() {
        let adapter = GeminiCliAdapter::new(Vec::new());
        let successor = adapter
            .parse_reader(
                std::io::Cursor::new(
                    concat!(
                        r#"{"sessionId":"family","kind":"main"}"#,
                        "\n",
                        r#"{"id":"shared","type":"user","content":"same"}"#,
                        "\n"
                    )
                    .as_bytes(),
                ),
                Path::new("session-family.jsonl"),
            )
            .unwrap();
        let predecessor = adapter
            .parse_raw(
                Path::new("session-family.json"),
                r#"{"sessionId":"family","kind":"main","messages":[
                    {"id":"shared","type":"user","content":"same"},
                    {"id":"old","type":"user","content":"legacy-only"}
                ]}"#
                .to_string(),
            )
            .unwrap();
        let merged = merge_migration_family(successor, predecessor).unwrap();
        assert!(merged
            .messages
            .iter()
            .any(|message| message.content == "legacy-only"));
        assert_eq!(
            merged
                .messages
                .iter()
                .filter(|message| message.content == "same")
                .count(),
            1
        );

        let changed_successor = adapter
            .parse_reader(
                std::io::Cursor::new(
                    concat!(
                        r#"{"sessionId":"family","kind":"main"}"#,
                        "\n",
                        r#"{"id":"shared","type":"user","content":"changed"}"#,
                        "\n"
                    )
                    .as_bytes(),
                ),
                Path::new("session-family.jsonl"),
            )
            .unwrap();
        let predecessor = adapter
            .parse_raw(
                Path::new("session-family.json"),
                r#"{"sessionId":"family","kind":"main","messages":[{"id":"shared","type":"user","content":"same"}]}"#
                    .to_string(),
            )
            .unwrap();
        let error = merge_migration_family(changed_successor, predecessor)
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicting content"), "{error}");
    }
}
