// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;

use crate::hashing::sha256;
use crate::models::{MessageCorrelationAuthority, ParsedSession, Provider, SourceFile};
use crate::providers::snapshot::{parsed_session_from_raw, source_file, SnapshotMetadata};
use crate::providers::{walk_roots, ProviderDiscovery};
use crate::util::{minimal_record, parse_datetime, RawMessage};

pub struct GeminiCliAdapter {
    roots: Vec<PathBuf>,
    project_paths: HashMap<String, String>,
}

impl GeminiCliAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let project_paths = roots
            .iter()
            .filter_map(|root| root.parent())
            .flat_map(known_project_paths)
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
        let walked = walk_roots(&self.roots, Some(3));
        for entry in walked.entries {
            let path = &entry.path;
            let supported = path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with("session-") && name.ends_with(".json"));
            let is_chat = path.parent().and_then(Path::file_name) == Some("chats".as_ref())
                && path
                    .strip_prefix(&entry.root)
                    .is_ok_and(|relative| relative.components().count() == 3);
            if supported && is_chat && entry.metadata.is_file() {
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
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read Gemini CLI session {}", path.display()))?;
        self.parse_raw(path, raw)
    }

    pub(crate) fn parse_raw(&self, path: &Path, raw: String) -> Result<ParsedSession> {
        let data: Value = serde_json::from_str(&raw).context("invalid Gemini CLI JSON")?;
        let strip_references = referenced_file_block_regex()?;
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
                messages.push(RawMessage::tool_call(name, args, call_id, call_ts));
                if let Some(result) = tool_result_text(call) {
                    messages.push(RawMessage::tool_result(name, result, call_id, call_ts));
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
        Ok(parsed_session_from_raw(
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
        ))
    }

    fn resolve_project_path(&self, path: &Path) -> Option<String> {
        let hash = path.parent()?.parent()?.file_name()?.to_str()?;
        self.project_paths.get(hash).cloned()
    }
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
            ("session-2026-02-23-id.json", "2026-02-23T00:00:00+00:00"),
            (
                "session-2026-02-23T04-07-id.json",
                "2026-02-23T04:07:00+00:00",
            ),
            (
                "session-2026-02-23T04-07-09-id.json",
                "2026-02-23T04:07:09+00:00",
            ),
        ];

        for (filename, expected) in cases {
            assert_eq!(
                timestamp_from_filename(Path::new(filename))
                    .unwrap()
                    .to_rfc3339(),
                expected
            );
        }
    }
}
