// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Adam Zhao
// SPDX-License-Identifier: Apache-2.0

use super::health::{RecordDisposition::Indexed, RecordTypeRule};

// Native discriminator inventory. Unlisted names are unknown and reported.
pub(crate) const RECORD_TYPES: &[RecordTypeRule] = &[
    ("user", Indexed("message role user")),
    ("assistant", Indexed("message role assistant")),
];

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::models::{FileEdit, ParsedSession, Provider, SessionRecord, SourceFile};
use crate::providers::{spawn, walk_roots, ProviderDiscovery};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    preview_from_text, substantive_text, truncate_for_display, RawMessage, UserRoleEvidence,
};

pub struct CursorAdapter {
    roots: Vec<PathBuf>,
}

impl CursorAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        self.discover_with_warnings().sources
    }

    pub(crate) fn discover_with_warnings(&self) -> ProviderDiscovery {
        let mut files = Vec::new();
        let walked = walk_roots(&self.roots, None);
        for entry in walked.entries {
            let path = &entry.path;
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            if !path
                .components()
                .any(|component| component.as_os_str() == "agent-transcripts")
            {
                continue;
            }
            // Nothing filters out `<session-id>/subagents/` here: those runs are sessions
            // of their own. See `spawn::subagents_dir_origin` for how their identity is
            // kept off the parent's row.
            let mtime_ns = entry
                .metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_nanos() as i64)
                .unwrap_or_default();
            files.push(SourceFile {
                sidecar: None,
                provider: Provider::Cursor,
                path: entry.path,
                mtime_ns,
                size_bytes: entry.metadata.len() as i64,
            });
        }
        ProviderDiscovery {
            sources: files,
            warnings: walked.warnings,
        }
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => minimal_record(Provider::Cursor, &source.path, format!("{err:#}")),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let file = std::fs::File::open(path)?;
        self.parse_reader(std::io::BufReader::new(file), path)
    }

    /// Parse cursor session lines from any reader. `parse_inner` calls this over the file; the
    /// incremental tail parser ([`crate::tail`]) calls it over an in-memory byte slice of the
    /// appended region, so the per-line logic lives in ONE place (a differential test asserts a
    /// tail parse equals a full parse). Streams line-by-line (task #241); `line_count` is tallied
    /// in this single pass. See claude::parse_reader notes.
    pub fn parse_reader<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
    ) -> Result<ParsedSession> {
        self.parse_reader_with_context(reader, path, None)
    }

    pub(crate) fn parse_reader_with_context<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
        context: Option<crate::tail::TailContext<'_>>,
    ) -> Result<ParsedSession> {
        let mut line_count: usize = 0;
        let mut malformed_line_count: usize = 0;
        let mut valid_record_count: usize = 0;
        let mut unrecognized = super::health::UnrecognizedRecordTypes::for_context(context);
        // A top-level cursor transcript is named for its session id, so the stem is the id. A
        // subagent transcript is named for its role instead (`subagents/subagent.jsonl` under
        // every parent), which is unique only within one parent — so it takes the parent's id
        // plus that name, and records the link. Same reasoning as claude.rs `ClaudeSpawn`.
        let spawned = spawn::subagents_dir_origin(path);
        let provider_session_id = match &spawned {
            Some(origin) => origin.session_id(),
            None => path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("unknown")
                .to_string(),
        };
        let cwd = infer_cursor_workspace(path);
        let mut created_at: Option<DateTime<Utc>> = None;
        let updated_at = file_modified_at(path);
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut pending_file_mutations = crate::files::PendingFileMutations::default();
        let mut requires_full_parse_for_tail = false;
        // tool_use_id -> tool name. Cursor transcripts use the Anthropic content-block shape
        // (verified against the kenn-io/agentsview parser), so a tool_result can be tagged
        // with the tool it came from, the same way the claude adapter does.
        let mut tool_use_names: HashMap<String, String> = HashMap::new();

        for line in crate::util::lines_with_byte_offsets(reader) {
            let (byte_offset, line) = line?;
            unrecognized.record(context, byte_offset);
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
            unrecognized.observe(Provider::Cursor, &value);
            let Some(role) = value.get("role").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            // Reuse the claude content-block helpers: capture file-mutating tool calls and
            // tag tool output. (Tool outputs are frequently absent from cursor transcripts on
            // disk; when present they are indexed as Role::Tool.)
            if let Some(message) = value.get("message") {
                super::claude::stage_file_edits(message, updated_at, &mut pending_file_mutations);
                super::claude::collect_file_edit_results(
                    message,
                    &mut pending_file_mutations,
                    &mut file_edits,
                );
                // A result outside the retained prefix can close a call before the overlap.
                // Ask the shared indexer to reparse rather than guessing its mutation bytes.
                if context.is_some_and(|c| c.prefix_truncated && byte_offset >= c.new_bytes_start)
                    && super::claude::is_tool_result(message)
                {
                    requires_full_parse_for_tail = true;
                }
                super::claude::collect_tool_use_names(message, &mut tool_use_names);
                super::claude::append_tool_use_messages(message, updated_at, &mut messages);
                if role == "user" {
                    if let Some(mixed) = super::claude::claude_mixed_user_content(
                        message,
                        &value,
                        spawned.is_some(),
                        &tool_use_names,
                        false,
                        None,
                    ) {
                        let (raw_message, transcript_text) = mixed.into_raw_message(updated_at);
                        messages.push(raw_message);
                        if !transcript_text.is_empty() {
                            transcript_lines.push(format_transcript_line(
                                "user",
                                updated_at,
                                &transcript_text,
                            ));
                        }
                        continue;
                    }
                }
                if role == "user" && super::claude::is_tool_result(message) {
                    let output = extract_text(message);
                    let output = output.trim();
                    if !output.is_empty() {
                        let tool_name = super::claude::tool_result_id(message)
                            .and_then(|id| tool_use_names.get(id).cloned());
                        messages.push(RawMessage::tool_result_with_name(
                            tool_name,
                            output.to_string(),
                            super::claude::tool_result_id(message),
                            updated_at,
                        ));
                    }
                    continue;
                }
            }
            let text = cursor_message_text(&value);
            if !substantive_text(&text) {
                continue;
            }
            if created_at.is_none() {
                created_at = updated_at;
            }
            messages.push(RawMessage::message(role, text.clone(), updated_at, None));
            transcript_lines.push(format_transcript_line(role, updated_at, &text));
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
        let title = last_user
            .clone()
            .or_else(|| first_user.clone())
            .map(|text| truncate_for_display(&text, 100));
        let preview = last_user
            .clone()
            .or_else(|| first_user.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());
        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let mut raw_metadata = json!({
            "line_count": line_count,
            "valid_record_count": valid_record_count,
            "session_path": normalize_path(path),
        });
        if malformed_line_count > 0 {
            raw_metadata["malformed_line_count"] = json!(malformed_line_count);
        }
        if requires_full_parse_for_tail {
            raw_metadata["requires_full_parse_for_tail"] = json!(true);
        }
        unrecognized.publish(&mut raw_metadata);
        let raw_metadata_json = Some(serde_json::to_string(&raw_metadata)?);

        let session = SessionRecord {
            id: format!("cursor:{provider_session_id}"),
            provider: Provider::Cursor,
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
            parse_version: crate::util::provider_parse_version(Provider::Cursor).to_string(),
            raw_metadata_json,
            parse_warning: super::malformed_jsonl_warning(malformed_line_count),
            discovery_source: "jsonl".to_string(),
            parent_session_id: spawned
                .as_ref()
                .map(|origin| origin.parent_link(Provider::Cursor)),
            // Cursor names the run rather than typing it, so the run's name is the only label
            // it records. Same slot as claude's `agentType` and codex's `agent_nickname`.
            agent_label: spawned.as_ref().map(|origin| origin.run_suffix.clone()),
        };

        let mut messages = crate::util::normalize_raw_messages_with_evidence(
            messages,
            None,
            if spawned.is_some() {
                UserRoleEvidence::AgentDelegationEvent
            } else {
                UserRoleEvidence::HumanInputEvent
            },
        );
        for message in &mut messages {
            cursor_input_provenance(message);
        }
        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages,
            file_edits,
            retracted: Default::default(),
        })
    }
}

fn cursor_message_text(value: &Value) -> String {
    let Some(message) = value.get("message") else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(content) = message.get("content") {
        collect_text_content(content, &mut parts);
    }
    let text = parts.join("\n");
    text.trim().to_string()
}

fn collect_text_content(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.trim().is_empty() {
                out.push(text.trim().to_string());
            }
        }
        Value::Array(items) => {
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.trim().is_empty() {
                            out.push(text.trim().to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

// Retain the entire searchable text. Only a single whole-turn wrapper proves an
// input region; inline, repeated, nested and incomplete tags are ambiguous.
fn cursor_query_region(text: &str) -> Option<(usize, usize)> {
    let body = text
        .strip_prefix("<user_query>")?
        .strip_suffix("</user_query>")?;
    if body.contains("<user_query>") || body.contains("</user_query>") {
        return None;
    }
    let start = "<user_query>".len() + body.len() - body.trim_start().len();
    Some((start, start + body.trim().len()))
}

// O(message bytes) time and partition metadata; text is not copied into parts.
fn cursor_input_provenance(message: &mut crate::models::Message) {
    use crate::models::{
        ContentPartAuthorship as A, ContentPartOrigin as O, MessageAuthorship, MessageContentPart,
        Role,
    };
    if message.role != Role::User {
        return;
    }
    let original = if message.provenance.content_parts.is_empty() {
        vec![MessageContentPart::new(
            0,
            0,
            message.content.chars().count(),
            if message.provenance.authorship == MessageAuthorship::Agent {
                A::Agent
            } else {
                A::Human
            },
            O::DirectInput,
        )]
    } else {
        std::mem::take(&mut message.provenance.content_parts)
    };
    let mut parts = Vec::new();
    let mut chars = message.content.char_indices().peekable();
    let mut position = 0;
    for part in original {
        let start_byte = chars
            .peek()
            .map_or(message.content.len(), |(offset, _)| *offset);
        while position < part.end_char {
            chars.next();
            position += 1;
        }
        let end_byte = chars
            .peek()
            .map_or(message.content.len(), |(offset, _)| *offset);
        let text = &message.content[start_byte..end_byte];
        let mut add = |start, end, authorship, origin| {
            if start < end {
                parts.push(MessageContentPart::new(
                    parts.len() as u32,
                    start,
                    end,
                    authorship,
                    origin,
                ));
            }
        };
        if part.origin != O::DirectInput || !text.contains("user_query") {
            add(part.start_char, part.end_char, part.authorship, part.origin);
        } else if let Some((start, end)) = cursor_query_region(text) {
            let end = part.start_char + text[..end].chars().count();
            let start = part.start_char + text[..start].chars().count();
            add(part.start_char, start, A::Harness, O::HarnessContext);
            add(start, end, part.authorship, O::DirectInput);
            add(end, part.end_char, A::Harness, O::HarnessContext);
        } else {
            add(part.start_char, part.end_char, A::Unknown, O::Unknown);
        }
    }
    message.provenance.authorship = if parts.len() == 1 {
        parts[0].authorship.into()
    } else {
        MessageAuthorship::Mixed
    };
    message.provenance.content_parts = parts;
}

fn file_modified_at(path: &Path) -> Option<DateTime<Utc>> {
    path.metadata()
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .map(DateTime::<Utc>::from)
}

fn infer_cursor_workspace(path: &Path) -> Option<String> {
    let mut previous: Option<String> = None;
    for component in path.components() {
        if component.as_os_str() == "agent-transcripts" {
            return previous
                .as_deref()
                .and_then(decode_cursor_project_dir)
                .map(|path| path.to_string_lossy().to_string());
        }
        previous = component.as_os_str().to_str().map(ToOwned::to_owned);
    }
    None
}

/// Resolve only a unique existing directory. At most 256 candidates are examined;
/// slugs needing more partitions return no workspace. For S slug bytes, time is
/// O(256 * S), memory O(S), and filesystem probes are at most 256.
fn decode_cursor_project_dir(encoded: &str) -> Option<PathBuf> {
    decode_cursor_project_dir_with(encoded, |candidate| {
        candidate.is_absolute() && candidate.is_dir()
    })
}

#[cfg(test)]
thread_local! {
    static WORKSPACE_CANDIDATES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn decode_cursor_project_dir_with(
    encoded: &str,
    mut is_dir: impl FnMut(&Path) -> bool,
) -> Option<PathBuf> {
    let parts: Vec<&str> = encoded.split('-').collect();
    let (root, suffix, separator) = match parts.as_slice() {
        [root @ ("Users" | "home"), suffix @ ..] if suffix.len() >= 2 => {
            (format!("/{root}"), suffix, "/")
        }
        [drive, "Users", suffix @ ..]
            if drive.len() == 1
                && drive.as_bytes()[0].is_ascii_alphabetic()
                && suffix.len() >= 2 =>
        {
            (format!("{drive}:\\Users"), suffix, "\\")
        }
        _ => return None,
    };
    // Refuse before shifting or allocating candidate partitions.
    if suffix.len() > 9 || suffix.iter().any(|part| part.is_empty()) {
        return None;
    }
    let mut found = None;
    for mask in 0..(1usize << (suffix.len() - 1)) {
        let mut candidate = root.clone();
        candidate.push_str(separator);
        candidate.push_str(suffix[0]);
        for (index, part) in suffix.iter().enumerate().skip(1) {
            candidate.push_str(if mask & (1 << (index - 1)) != 0 {
                separator
            } else {
                "-"
            });
            candidate.push_str(part);
        }
        let candidate = PathBuf::from(candidate);
        #[cfg(test)]
        WORKSPACE_CANDIDATES.with(|count| count.set(count.get() + 1));
        if is_dir(&candidate) {
            if found.is_some() {
                return None;
            }
            found = Some(candidate);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::{cursor_message_text, decode_cursor_project_dir, CursorAdapter};
    use crate::models::Provider;
    use serde_json::{json, Value};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discovers_and_parses_cursor_parent_transcripts() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "9f3b844f-072d-4c2a-bdb8-474fe89dbca1";
        let transcript_dir = root
            .join("Users-adamzhao-Desktop-aise")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcript_dir).expect("create transcript dir");
        let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<timestamp>Tuesday</timestamp>\n<user_query>\nMake Cursor threads searchable\n</user_query>"}]}}
{"role":"assistant","message":{"content":[{"type":"text","text":"I will wire a Cursor provider."},{"type":"tool_use","name":"ReadFile","input":{"path":"/tmp/nope"}}]}}
{"role":"user","message":{"content":[{"type":"text","text":"Great, add tests too"}]}}
"#,
        )
        .expect("write transcript");

        let subagent_dir = transcript_dir.join("subagents");
        fs::create_dir_all(&subagent_dir).expect("create subagent dir");
        fs::write(
            subagent_dir.join("subagent.jsonl"),
            r#"{"role":"user","message":{"content":[{"type":"text","text":"subagent"}]}}"#,
        )
        .expect("write subagent transcript");

        let adapter = CursorAdapter::new(vec![root]);
        let sources = adapter.discover();
        // The subagent transcript is a session of its own; this test covers the parent, and
        // `a_cursor_subagent_is_indexed_under_its_parents_id` covers the spawned run.
        assert_eq!(sources.len(), 2, "{sources:#?}");
        let parent_source = sources
            .iter()
            .find(|source| source.path == transcript_path)
            .expect("the parent transcript is discovered");
        assert_eq!(parent_source.provider, Provider::Cursor);

        let parsed = adapter.parse(parent_source);
        assert_eq!(parsed.session.id, format!("cursor:{session_id}"));
        assert_eq!(parsed.session.provider_session_id, session_id);
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Great, add tests too")
        );
        assert_eq!(
            parsed.session.summary.as_deref(),
            Some("<timestamp>Tuesday</timestamp> <user_query> Make Cursor threads searchable </user_query>")
        );
        assert_eq!(parsed.session.message_count, Some(4));
        assert!(parsed
            .transcript_text
            .contains("Make Cursor threads searchable"));
        assert!(parsed
            .transcript_text
            .contains("I will wire a Cursor provider."));
        assert!(!parsed.transcript_text.contains("ReadFile"));
        assert!(!parsed.transcript_text.contains("subagent"));
        assert!(parsed.messages.iter().any(|m| {
            m.tool_name.as_deref() == Some("ReadFile")
                && m.content.contains(r#""kind":"tool_call""#)
                && m.content.contains(r#""path":"/tmp/nope""#)
        }));
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Unknown
        );
        assert_eq!(
            parsed.messages[2].provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
    }

    /// Cursor's subagent transcripts are named for their role rather than with an id, so two
    /// sessions' subagents collide on the filename alone — `subagents/subagent.jsonl` under
    /// every parent. The id is therefore the parent's plus the run's name, which is unique by
    /// construction. Asserting the owner of each id, not just that the two differ.
    #[test]
    fn a_cursor_subagent_is_indexed_under_its_parents_id() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let parents = [
            "9f3b844f-072d-4c2a-bdb8-474fe89dbca1",
            "1c0dd0f2-6f4f-4a0e-8f01-9b1c6a4a44b7",
        ];
        for parent in parents {
            let transcript_dir = root
                .join("Users-adamzhao-Desktop-aise")
                .join("agent-transcripts")
                .join(parent);
            fs::create_dir_all(transcript_dir.join("subagents")).expect("create dirs");
            fs::write(
                transcript_dir.join(format!("{parent}.jsonl")),
                r#"{"role":"user","message":{"content":[{"type":"text","text":"parent turn"}]}}"#,
            )
            .expect("write parent transcript");
            fs::write(
                transcript_dir.join("subagents").join("subagent.jsonl"),
                format!(
                    r#"{{"role":"user","message":{{"content":[{{"type":"text","text":"run for {parent}"}}]}}}}"#
                ),
            )
            .expect("write subagent transcript");
        }

        let adapter = CursorAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 4, "two parents and two runs: {sources:#?}");

        for parent in parents {
            let source = sources
                .iter()
                .find(|source| {
                    source.path.ends_with("subagent.jsonl")
                        && source.path.to_string_lossy().contains(parent)
                })
                .expect("each parent's subagent transcript is discovered");
            let parsed = adapter.parse(source);
            assert_eq!(
                parsed.session.provider_session_id,
                format!("{parent}/subagent"),
                "a name shared by every parent's run has to be qualified to stay distinct"
            );
            assert_eq!(
                parsed.session.parent_session_id.as_deref(),
                Some(format!("cursor:{parent}").as_str())
            );
            assert_eq!(parsed.session.agent_label.as_deref(), Some("subagent"));
            assert!(parsed
                .transcript_text
                .contains(&format!("run for {parent}")));
            assert_eq!(
                parsed.messages[0].provenance.authorship,
                crate::models::MessageAuthorship::Agent
            );
        }
    }

    #[test]
    fn literal_query_tags_keep_all_text_and_do_not_prove_human_input() {
        let adapter = CursorAdapter::new(vec![]);
        for role in ["user", "assistant"] {
            for text in [
                "Explain <user_query>x</user_query> syntax",
                "<user_query>x</user_query> after <user_query>y</user_query>",
            ] {
                let value = json!({"role":role,"message":{"content":text}});
                assert_eq!(cursor_message_text(&value), text);
                let parsed = adapter
                    .parse_reader(
                        std::io::Cursor::new(value.to_string()),
                        std::path::Path::new("synthetic.jsonl"),
                    )
                    .unwrap();
                assert_eq!(parsed.messages[0].content, text);
                assert_ne!(
                    parsed.messages[0].provenance.authorship,
                    crate::models::MessageAuthorship::Human
                );
            }
        }
    }

    #[test]
    fn extracts_user_query_from_cursor_message() {
        let value = json!({
            "role": "user",
            "message": {
                "content": [{
                    "type": "text",
                    "text": "<timestamp>Tuesday</timestamp>\n<user_query>\nFind the billing bug\n</user_query>"
                }]
            }
        });
        assert_eq!(
            cursor_message_text(&value),
            "<timestamp>Tuesday</timestamp>\n<user_query>\nFind the billing bug\n</user_query>"
        );
    }

    #[test]
    fn whole_query_wrapper_selects_only_its_unicode_body() {
        let text = "<user_query>\nrequest 🦀\n</user_query>";
        let adapter = CursorAdapter::new(vec![]);
        let value = json!({"role":"user","message":{"content":text}});
        let parsed = adapter
            .parse_reader(
                std::io::Cursor::new(value.to_string()),
                std::path::Path::new("synthetic.jsonl"),
            )
            .unwrap();
        let message = &parsed.messages[0];
        assert_eq!(message.content, text);
        message.provenance.validate(text).unwrap();
        let human: String = message
            .provenance
            .content_parts
            .iter()
            .filter(|part| part.authorship == crate::models::ContentPartAuthorship::Human)
            .flat_map(|part| {
                text.chars()
                    .skip(part.start_char)
                    .take(part.end_char - part.start_char)
            })
            .collect();
        assert_eq!(human, "request 🦀");
        assert_eq!(message.provenance.content_parts.len(), 3);
    }

    #[test]
    fn ignores_tool_use_payloads_in_cursor_messages() {
        let value = json!({
            "role": "assistant",
            "message": {
                "content": [
                    {"type": "text", "text": "I found it."},
                    {"type": "tool_use", "name": "ReadFile", "input": {"path": "/tmp/secret"}}
                ]
            }
        });
        assert_eq!(cursor_message_text(&value), "I found it.");
    }

    #[test]
    fn mutation_result_in_later_refresh_commits_and_truncated_prefix_requests_reparse() {
        use std::io::Write;
        let temp = tempdir().unwrap();
        let path = temp.path().join("synthetic.jsonl");
        let adapter = CursorAdapter::new(vec![]);
        let call = json!({"role":"assistant","message":{"content":[
            {"type":"tool_use","id":"write","name":"Write","input":{"file_path":"ok.txt","content":"exact bytes"}}
        ]}});
        let result = json!({"role":"user","message":{"content":[
            {"type":"tool_result","tool_use_id":"write","content":"done"}
        ]}});
        fs::write(&path, format!("{call}\n")).unwrap();
        let before = adapter.parse_inner(&path).unwrap();
        assert!(before.file_edits.is_empty());
        let offset = crate::tail::complete_prefix_offset(&path).unwrap();
        writeln!(
            fs::OpenOptions::new().append(true).open(&path).unwrap(),
            "{result}"
        )
        .unwrap();
        let crate::tail::TailParseRun::Complete(Some(tail)) = crate::tail::tail_parse_until(
            &path,
            offset,
            None,
            &|| false,
            |reader, path, context| adapter.parse_reader_with_context(reader, path, Some(context)),
        )
        .unwrap() else {
            panic!("a correlated append must remain tail-parseable")
        };
        assert_eq!(tail.new_file_edits.len(), 1);
        assert_eq!(
            tail.new_file_edits[0].new_content.as_deref(),
            Some("exact bytes")
        );
        let full = adapter.parse_inner(&path).unwrap();
        assert_eq!(tail.new_file_edits, full.file_edits);
        let suffix = adapter
            .parse_reader_with_context(
                std::io::Cursor::new(result.to_string()),
                &path,
                Some(crate::tail::TailContext {
                    prefix_truncated: true,
                    ..Default::default()
                }),
            )
            .unwrap();
        assert!(suffix.file_edits.is_empty());
        let metadata: Value =
            serde_json::from_str(suffix.session.raw_metadata_json.as_deref().unwrap()).unwrap();
        assert_eq!(metadata["requires_full_parse_for_tail"], true);
    }

    #[test]
    fn cursor_file_edits_require_correlated_success() {
        let adapter = CursorAdapter::new(vec![]);
        let records = [
            json!({"role":"assistant","message":{"content":[
                {"type":"tool_use","id":"failed","name":"Write","input":{"file_path":"failed.txt","content":"never written"}},
                {"type":"tool_use","id":"pending","name":"Write","input":{"file_path":"pending.txt","content":"unproven"}},
                {"type":"tool_use","id":"success","name":"Write","input":{"file_path":"success.txt","content":"proven bytes"}}
            ]}}),
            json!({"role":"user","message":{"content":[
                {"type":"tool_result","tool_use_id":"failed","is_error":true,"content":"denied"},
                {"type":"tool_result","tool_use_id":"unrelated","content":"ok"},
                {"type":"tool_result","tool_use_id":"success","is_error":false,"content":"ok"}
            ]}}),
        ];
        let text = records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let parsed = adapter
            .parse_reader(
                std::io::Cursor::new(text),
                std::path::Path::new("synthetic.jsonl"),
            )
            .unwrap();
        assert_eq!(parsed.file_edits.len(), 1);
        assert_eq!(parsed.file_edits[0].file_path, "success.txt");
        assert_eq!(
            parsed.file_edits[0].new_content.as_deref(),
            Some("proven bytes")
        );
        assert!(parsed
            .messages
            .iter()
            .any(|m| m.content.contains("never written")));
    }

    #[test]
    fn indexes_cursor_tool_results_and_file_edits() {
        use crate::models::Role;
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "11111111-2222-3333-4444-555555555555";
        let dir = root
            .join("Users-x-proj")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &path,
            concat!(
                r#"{"role":"user","message":{"content":[{"type":"text","text":"edit the file"}]}}"#,
                "\n",
                r#"{"role":"assistant","message":{"content":[{"type":"text","text":"editing"},{"type":"tool_use","id":"tu_e","name":"Edit","input":{"file_path":"/p/app.ts","old_string":"a","new_string":"b"}},{"type":"tool_use","id":"tu_p","name":"ApplyPatch","input":{"path":"/p/x.ts","patch":"@@ -1 +1 @@"}}]}}"#,
                "\n",
                r#"{"role":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu_e","content":"edit applied"}]}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = CursorAdapter::new(vec![root]);
        let sources = adapter.discover();
        let parsed = adapter.parse(&sources[0]);

        // Only the Edit has a successful result; ApplyPatch remains attempted activity.
        let files: Vec<&str> = parsed
            .file_edits
            .iter()
            .map(|e| e.file_name.as_str())
            .collect();
        assert!(files.contains(&"app.ts"), "Edit file recorded: {files:?}");
        assert!(!files.contains(&"x.ts"));
        assert_eq!(files.len(), 1);
        let tool_input = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content.contains(r#""old_string":"a""#))
            .expect("tool_use input indexed as a Role::Tool message");
        assert_eq!(tool_input.tool_name.as_deref(), Some("Edit"));
        assert_eq!(tool_input.kind, crate::models::MessageKind::ToolCall);
        assert_eq!(tool_input.tool_call_id.as_deref(), Some("tu_e"));
        // The tool_result is indexed as a separate Role::Tool message tagged with the Edit tool.
        let tool_result = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool && m.content == "edit applied")
            .expect("tool_result indexed as a Role::Tool message");
        assert_eq!(tool_result.tool_name.as_deref(), Some("Edit"));
        assert_eq!(tool_result.kind, crate::models::MessageKind::ToolResult);
        assert_eq!(tool_result.tool_call_id.as_deref(), Some("tu_e"));
        // Tool payloads stay out of the human transcript.
        assert!(!parsed.transcript_text.contains("edit applied"));
        assert!(!parsed.transcript_text.contains("ApplyPatch"));
    }

    #[test]
    fn mixed_cursor_user_blocks_keep_direct_input_and_tool_output_separate() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "22222222-3333-4444-5555-666666666666";
        let dir = root
            .join("Users-x-proj")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &path,
            concat!(
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","id":"tu_read","name":"Read","input":{}}]}}"#,
                "\n",
                r#"{"role":"user","message":{"content":[{"type":"text","text":"direct cursor input 🦀"},{"type":"tool_result","tool_use_id":"tu_read","content":"generated tool output"}]}}"#,
                "\n",
            ),
        )
        .unwrap();

        let adapter = CursorAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        let message = parsed
            .messages
            .iter()
            .find(|message| message.role == crate::models::Role::User)
            .expect("mixed user record remains one conversation row");
        assert_eq!(
            message.content,
            "direct cursor input 🦀\ngenerated tool output"
        );
        assert_eq!(
            message.provenance.authorship,
            crate::models::MessageAuthorship::Mixed
        );
        assert!(message.provenance.content_parts.iter().any(|part| {
            part.authorship == crate::models::ContentPartAuthorship::Human
                && part.origin == crate::models::ContentPartOrigin::DirectInput
        }));
        assert!(message.provenance.content_parts.iter().any(|part| {
            part.authorship == crate::models::ContentPartAuthorship::Generated
                && part.origin == crate::models::ContentPartOrigin::ToolPayload
        }));
        message.provenance.validate(&message.content).unwrap();
        assert!(parsed.transcript_text.contains("direct cursor input 🦀"));
        assert!(!parsed.transcript_text.contains("generated tool output"));
        let db = crate::db::Db::open(&temp.path().join("index.db")).unwrap();
        db.mark_schema_current().unwrap();
        let mut old = parsed.clone();
        old.session.parse_version = "cursor-v4".into();
        let old_mixed = old
            .messages
            .iter_mut()
            .find(|message| message.role == crate::models::Role::User)
            .unwrap();
        old_mixed.role = crate::models::Role::Tool;
        old_mixed.kind = crate::models::MessageKind::ToolResult;
        old_mixed.provenance.authorship = crate::models::MessageAuthorship::Generated;
        old_mixed.provenance.content_parts.clear();
        old_mixed.provenance.input_origin = None;
        db.upsert_session(&old, 0, 0).unwrap();
        let stored = rusqlite::Connection::open(temp.path().join("index.db")).unwrap();
        let old_id: i64 = stored
            .query_row(
                "select id from messages where kind='tool_result'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        db.upsert_session(&parsed, 0, 0).unwrap();
        let new_id: i64 = stored
            .query_row("select id from messages where role='user'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            old_id, new_id,
            "mixed-role upgrade retains the original message reference"
        );
        assert_eq!(
            db.message_count().unwrap(),
            i64::try_from(parsed.messages.len()).unwrap()
        );
        let config = crate::config::Config::default();
        let service = crate::service::MessageService::new(
            &config,
            &db,
            crate::message_search::SearchSurface::Rust,
        );
        for (query, expected) in [("direct cursor input 🦀", 1), ("generated tool output", 0)] {
            let request = crate::message_search::MessageSearchRequest::builder(
                crate::message_search::MessageQuery::literal(query).unwrap(),
                crate::message_search::MessageTarget::content(),
            )
            .role(crate::models::Role::User)
            .build()
            .unwrap();
            assert_eq!(service.search(request).unwrap().hits().len(), expected);
        }
    }

    #[test]
    fn cursor_does_not_promote_claude_native_tool_use_result_fields() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "33333333-4444-5555-6666-777777777777";
        let dir = root
            .join("Users-x-proj")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &path,
            concat!(
                r#"{"role":"assistant","message":{"content":[{"type":"tool_use","id":"ask-cursor","name":"AskUserQuestion","input":{}}]}}"#,
                "\n",
                r#"{"role":"user","message":{"content":[{"type":"tool_result","tool_use_id":"ask-cursor","content":"The user responded: cursor wrapper"}]},"tool_use_result":{"response":"cursor native answer"}}"#,
                "\n",
            ),
        )
        .unwrap();
        let adapter = CursorAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert!(parsed
            .messages
            .iter()
            .all(|message| !message.content.contains("cursor native answer")));
        assert!(parsed
            .messages
            .iter()
            .any(|message| message.content.contains("cursor wrapper")));
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
            .search(request("cursor native answer"))
            .unwrap()
            .hits()
            .is_empty());
        assert!(service
            .search(request("cursor wrapper"))
            .unwrap()
            .hits()
            .is_empty());
    }

    #[test]
    fn workspace_candidates_have_a_fixed_bound() {
        for length in [4, 8, 12, 25, 50] {
            super::WORKSPACE_CANDIDATES.with(|count| count.set(0));
            let slug = format!("Users-{}", vec!["aisemissing"; length].join("-"));
            assert!(decode_cursor_project_dir(&slug).is_none());
            let count = super::WORKSPACE_CANDIDATES.with(|count| count.get());
            assert!(
                count <= 256,
                "{length} components examined {count} candidates, maximum 256"
            );
        }
    }

    #[test]
    fn workspace_roots_and_ambiguity_are_host_independent() {
        for (slug, expected) in [
            ("Users-person-project", "/Users/person/project"),
            ("home-person-project", "/home/person/project"),
            ("C-Users-person-project", "C:\\Users\\person\\project"),
        ] {
            let expected = std::path::PathBuf::from(expected);
            assert_eq!(
                super::decode_cursor_project_dir_with(slug, |path| path == expected),
                Some(expected)
            );
            assert!(super::decode_cursor_project_dir_with(slug, |_| false).is_none());
            assert!(super::decode_cursor_project_dir_with(slug, |_| true).is_none());
        }
    }

    #[test]
    fn skips_empty_window_workspace() {
        assert!(decode_cursor_project_dir("empty-window").is_none());
    }

    /// Differential guard for the streaming-parse refactor (task #241): identical output
    /// and `line_count` between the streaming `BufReader` path and the prior whole-file
    /// `fs::read_to_string` path. Fixture has a blank line, a malformed line, and a final
    /// line without a trailing newline (line_count must be 5).
    #[test]
    fn streaming_parse_output_is_stable() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "9f3b844f-072d-4c2a-bdb8-474fe89dbca1";
        let transcript_dir = root
            .join("Users-x-Desktop-proj")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcript_dir).expect("create transcript dir");
        let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
        // 5 physical lines, no trailing newline on the last:
        //   1 user  2 malformed (skipped)  3 blank  4 assistant+tool_use  5 user (no \n)
        let content = concat!(
            r#"{"role":"user","message":{"content":[{"type":"text","text":"<user_query>\nfirst\n</user_query>"}]}}"#,
            "\n",
            "{bad json\n",
            "\n",
            r#"{"role":"assistant","message":{"content":[{"type":"text","text":"on it"},{"type":"tool_use","name":"Write","input":{"file_path":"/p/a.rs","content":"hello"}}]}}"#,
            "\n",
            r#"{"role":"user","message":{"content":[{"type":"text","text":"second"}]}}"#,
        );
        fs::write(&transcript_path, content).expect("write transcript");

        let adapter = CursorAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);

        assert!(
            parsed
                .session
                .raw_metadata_json
                .as_deref()
                .unwrap()
                .contains("\"line_count\":5"),
            "line_count must be 5, got: {:?}",
            parsed.session.raw_metadata_json
        );
        assert_eq!(parsed.session.provider, Provider::Cursor);
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(
            contents,
            vec![
                "<user_query>\nfirst\n</user_query>",
                r#"{"args":{"content":"hello","file_path":"/p/a.rs"},"kind":"tool_call","tool_name":"Write"}"#,
                "on it",
                "second"
            ]
        );
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "tool", "assistant", "user"]);
        assert!(parsed.file_edits.is_empty());
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
    }

    /// Non-UTF-8 bytes must never panic or abort the parse — they are decoded lossily (U+FFFD).
    /// This input is not valid JSON even after lossy decoding, so it yields no messages, but
    /// parsing completes WITHOUT error (lossy recovery is not treated as a parse failure).
    #[test]
    fn non_utf8_garbage_is_diagnosed_without_aborting_the_parse() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "9f3b844f-072d-4c2a-bdb8-474fe89dbca1";
        let transcript_dir = root
            .join("Users-x-Desktop-proj")
            .join("agent-transcripts")
            .join(session_id);
        fs::create_dir_all(&transcript_dir).expect("create transcript dir");
        let transcript_path = transcript_dir.join(format!("{session_id}.jsonl"));
        fs::write(&transcript_path, [b'{', 0xFF, 0xFE, b'}', b'\n']).expect("write");

        let adapter = CursorAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        assert!(parsed.messages.is_empty());
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
        assert_eq!(parsed.session.message_count, Some(0));
    }
}
