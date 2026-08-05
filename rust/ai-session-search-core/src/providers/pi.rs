// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-FileCopyrightText: 2026 Nisarg Patel
// SPDX-License-Identifier: Apache-2.0

use std::path::{Component, Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};

use crate::files::{FileMutationPayload, PendingFileMutations};
use crate::models::{
    EditOp, FileEdit, MessageCorrelationAuthority, ParsedSession, Provider, SessionRecord,
    SourceFile,
};
use crate::providers::{spawn::SpawnOrigin, walk_roots, ProviderDiscovery};
use crate::util::{
    apply_user_role_authorship, extract_text, find_repo_root, format_transcript_line,
    minimal_record, normalize_path, parse_datetime, preview_from_text, substantive_text,
    truncate_for_display, RawMessage, UserRoleAuthorshipEvidence,
};

pub struct PiAdapter {
    roots: Vec<PathBuf>,
    id_re: Regex,
}

impl PiAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            id_re: Regex::new(r"([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})")
                .expect("valid regex"),
        }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        self.discover_with_warnings().sources
    }

    pub(crate) fn discover_with_warnings(&self) -> ProviderDiscovery {
        let mut files = Vec::new();
        let walked = walk_roots(&self.roots, None);
        for entry in walked.entries {
            let root = &entry.root;
            let path = &entry.path;
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            // Top-level project sessions live directly under <root>/<encoded-cwd>/<file>.jsonl.
            // Subagent runs are nested deeper (<encoded-cwd>/<session>/<agent>/run-N/
            // session.jsonl) and are sessions of their own. Anything nested that names no
            // parent session directory is neither, so it stays out rather than becoming a
            // session with an invented identity.
            if !is_top_level_session(root, path) && self.spawn_origin(root, path).is_none() {
                continue;
            }
            let mtime_ns = entry
                .metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_nanos() as i64)
                .unwrap_or_default();
            files.push(SourceFile {
                provider: Provider::Pi,
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
            Err(err) => minimal_record(Provider::Pi, &source.path, format!("{err:#}")),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let file = std::fs::File::open(path)?;
        self.parse_reader(std::io::BufReader::new(file), path)
    }

    /// Parse pi session lines from any reader. `parse_inner` calls this over the file; the
    /// incremental tail parser ([`crate::tail`]) calls it over an in-memory byte slice of the
    /// appended region, so the per-line logic lives in ONE place (a differential test asserts a
    /// tail parse equals a full parse). Streams line-by-line (task #241); `line_count` is tallied
    /// in this single pass. See claude::parse_reader notes.
    pub fn parse_reader<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
    ) -> Result<ParsedSession> {
        let mut line_count: usize = 0;
        let mut malformed_line_count: usize = 0;
        // Pi gives a spawned run a session id from the same space as a top-level one, so the
        // `session` record below binds it either way and no parent qualification is needed —
        // unlike claude, whose subagent records carry only the PARENT's id. The origin is used
        // for the fallback: two runs whose transcripts name no id would otherwise share one
        // placeholder and upsert onto each other.
        let spawned = self.spawn_origin_of(path);
        let mut provider_session_id = spawned
            .as_ref()
            .map(SpawnOrigin::session_id)
            .or_else(|| self.extract_id(path))
            .unwrap_or_else(|| "unknown".to_string());
        // Bind the session id once; see the guard on the `session` record below.
        let mut session_id_bound = false;
        let mut cwd = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut pending_file_mutations = PendingFileMutations::default();

        for line in crate::util::lines_replacing_invalid_utf8(reader) {
            let line = line?;
            line_count += 1;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(_) => {
                    if !line.contains(char::REPLACEMENT_CHARACTER) {
                        malformed_line_count += 1;
                    }
                    continue;
                }
            };

            let timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);

            match value.get("type").and_then(Value::as_str) {
                Some("session") => {
                    // First `session` record wins, matching cwd/created_at below. A later
                    // record naming a different id would retarget this file, and the
                    // `on conflict(id) do update` upsert in db.rs would overwrite that
                    // other session's row rather than storing this one.
                    if !session_id_bound {
                        if let Some(id) = value.get("id").and_then(Value::as_str) {
                            provider_session_id = id.to_string();
                        }
                        session_id_bound = true;
                    }
                    if cwd.is_none() {
                        cwd = value
                            .get("cwd")
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned);
                    }
                    created_at = created_at.or(timestamp);
                }
                Some("message") => {
                    let Some(message) = value.get("message") else {
                        continue;
                    };
                    let role = message.get("role").and_then(Value::as_str);
                    // Capture file-mutating `toolCall` blocks before the empty-text skip,
                    // so a tool-only assistant turn (no text) still records its edits.
                    if role == Some("assistant") {
                        collect_pi_file_edits(message, timestamp, &mut pending_file_mutations);
                        append_pi_tool_calls(message, timestamp, &mut messages);
                    } else if role == Some("toolResult") {
                        // Pi's persisted ToolResultMessage has an explicit `isError` boolean.
                        // Missing/non-boolean flags are unknown, so they cannot prove a mutation.
                        pending_file_mutations.finish(
                            message.get("toolCallId").and_then(Value::as_str),
                            message.get("isError").and_then(Value::as_bool) == Some(false),
                            &mut file_edits,
                        );
                    }
                    let text = extract_text(message);
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    match role {
                        Some("user") | Some("assistant") => {
                            if created_at.is_none() {
                                created_at = timestamp;
                            }
                            updated_at = timestamp.or(updated_at);
                            let mut raw_message = RawMessage::message(
                                role.unwrap_or("message"),
                                text.to_string(),
                                timestamp,
                                None,
                            );
                            if let Some(event_id) = value
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|id| !id.trim().is_empty())
                            {
                                raw_message = raw_message.with_native_event_identity(
                                    MessageCorrelationAuthority::Pi,
                                    event_id,
                                );
                            }
                            messages.push(raw_message);
                            transcript_lines.push(format_transcript_line(
                                role.unwrap_or("message"),
                                timestamp,
                                text,
                            ));
                        }
                        // Tool output: index as a Role::Tool message (searchable via
                        // `messages search --role tool`), tagged with the tool's name, but
                        // kept out of the conversation transcript/title/preview.
                        Some("toolResult") => {
                            updated_at = timestamp.or(updated_at);
                            let tool_name = message
                                .get("toolName")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            let mut raw_message = RawMessage::tool_result_with_name(
                                tool_name,
                                text.to_string(),
                                message.get("toolCallId").and_then(Value::as_str),
                                timestamp,
                            );
                            if let Some(event_id) = value
                                .get("id")
                                .and_then(Value::as_str)
                                .filter(|id| !id.trim().is_empty())
                            {
                                raw_message = raw_message.with_native_event_identity(
                                    MessageCorrelationAuthority::Pi,
                                    event_id,
                                );
                            }
                            messages.push(raw_message);
                        }
                        _ => continue,
                    }
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
        let title = first_user
            .clone()
            .or_else(|| last_user.clone())
            .map(|text| truncate_for_display(&text, 100));
        let summary = first_user
            .clone()
            .map(|text| truncate_for_display(&text, 180));
        let preview = last_user
            .clone()
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
        let raw_metadata_json = Some(serde_json::to_string(&raw_metadata)?);

        let session = SessionRecord {
            id: format!("pi:{provider_session_id}"),
            provider: Provider::Pi,
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
            parse_version: crate::util::provider_parse_version(Provider::Pi).to_string(),
            raw_metadata_json,
            parse_warning: super::malformed_jsonl_warning(malformed_line_count),
            discovery_source: "jsonl".to_string(),
            parent_session_id: spawned
                .as_ref()
                .map(|origin| origin.parent_link(Provider::Pi)),
            // Pi names the run's directory after the agent and nests each attempt below it
            // (`<agent>/run-N/`), so the leading segment is the agent and the rest is which
            // attempt. Same slot as claude's `agentType` and codex's `agent_nickname`.
            agent_label: spawned
                .as_ref()
                .and_then(|origin| origin.run_suffix.split('/').next().map(ToOwned::to_owned)),
        };

        let mut messages = crate::util::to_messages_with_tools_in_scope(messages, &session.id);
        apply_user_role_authorship(
            &mut messages,
            if spawned.is_some() {
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

    fn extract_id(&self, path: &Path) -> Option<String> {
        self.id_in(path.file_stem().and_then(|stem| stem.to_str())?)
    }

    /// The session id embedded in a file or directory name. Pi names both after the session
    /// they hold (`2026-06-18T17-31-17-343Z_<id>`), which is what lets a run find its parent.
    fn id_in(&self, name: &str) -> Option<String> {
        self.id_re
            .captures(name)
            .and_then(|captures| captures.get(1))
            .map(|match_| match_.as_str().to_string())
    }

    /// Which configured root a transcript sits under. `discover` walks from a root and knows
    /// it; `parse` is handed only the path, so it resolves the root back here. The longest
    /// match wins, so a root configured inside another still resolves to the specific one.
    fn root_for(&self, path: &Path) -> Option<&Path> {
        self.roots
            .iter()
            .filter(|root| path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .map(PathBuf::as_path)
    }

    /// Read spawn origin off a nested pi path: `<parent-session-dir>/<agent>/run-N/
    /// session.jsonl`, where the parent's directory is named for the session it belongs to.
    ///
    /// Walks up to the nearest ancestor directory whose name yields a session id and takes
    /// everything below it as what distinguishes the run — the same shape claude and cursor
    /// get from their `subagents` marker directory, derived differently because pi has none.
    /// A nested file with no such ancestor is not a run of anything and yields `None`.
    fn spawn_origin(&self, root: &Path, path: &Path) -> Option<SpawnOrigin> {
        let relative = path.strip_prefix(root).ok()?;
        let names: Vec<&str> = relative
            .components()
            .filter_map(|component| match component {
                Component::Normal(name) => name.to_str(),
                _ => None,
            })
            .collect();
        let (file, directories) = names.split_last()?;
        let (index, parent_session_id) = directories
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, name)| self.id_in(name).map(|id| (index, id)))?;
        let mut run: Vec<&str> = directories[index + 1..].to_vec();
        if run.is_empty() {
            // The transcript sits directly in the session directory, so it IS that session
            // rather than a run spawned by it.
            return None;
        }
        run.push(Path::new(file).file_stem()?.to_str()?);
        Some(SpawnOrigin {
            parent_session_id,
            run_suffix: run.join("/"),
        })
    }

    /// [`Self::spawn_origin`] for a path whose root has to be resolved first.
    fn spawn_origin_of(&self, path: &Path) -> Option<SpawnOrigin> {
        self.spawn_origin(self.root_for(path)?, path)
    }
}

/// A session file is "top level" when it sits at most one directory below the
/// configured root. With the default root (`~/.pi/agent/sessions`) that's
/// `<root>/<encoded-cwd>/<file>.jsonl` (depth 2); if a user points a root at a
/// specific project directory the session files sit directly in it (depth 1).
/// Subagent transcripts live further down the tree (`<session>/<agent>/run-N/
/// session.jsonl`) and are excluded so they don't duplicate the parent session.
fn is_top_level_session(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    matches!(relative.components().count(), 1 | 2)
}

/// Scan a pi assistant `message.content` array for `toolCall` blocks that mutate a file
/// (`write`/`edit`) and append a [`FileEdit`] for each, assigning monotonic session-local
/// sequence numbers. Sparse upstream event indexes are ignored because edit sequence numbers are
/// local to aise's file-recovery stream. The two file-mutating tools are the only ones in
/// pi's built-in set (`read|bash|edit|write|grep|find|ls`); everything else is skipped.
fn append_pi_tool_calls(
    message: &Value,
    timestamp: Option<DateTime<Utc>>,
    messages: &mut Vec<RawMessage>,
) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
            continue;
        }
        let Some(name) = block.get("name").and_then(Value::as_str) else {
            continue;
        };
        messages.push(RawMessage::tool_call(
            name,
            block.get("arguments").cloned().unwrap_or(Value::Null),
            block.get("id").and_then(Value::as_str),
            timestamp,
        ));
    }
}

fn collect_pi_file_edits(
    message: &Value,
    ts: Option<DateTime<Utc>>,
    pending: &mut PendingFileMutations,
) {
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
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
            pi_tool_edit_payload(name, block.get("arguments")),
        );
    }
}

/// Map a single pi `write`/`edit` toolCall to `(file_path, full_content?, edits)`.
/// `write` yields a full-content snapshot (replayable via `files extract`); `edit` yields
/// `old`→`new` delta ops. Pi's `edit` arguments appear in TWO shapes in the wild and BOTH
/// must be accepted (confirmed by pi's own `edit-tool-legacy-input.test.ts`):
///   - legacy flat: `{path, oldText, newText}`
///   - current nested: `{path, edits: [{oldText, newText}, ...]}`
fn pi_tool_edit_payload(name: &str, args: Option<&Value>) -> Option<FileMutationPayload> {
    let args = args?;
    let str_field = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
    match name {
        "write" => {
            let path = str_field("path")?;
            let content = str_field("content").unwrap_or_default();
            Some((path, Some(content), Vec::new()))
        }
        "edit" => {
            let path = str_field("path")?;
            // Current nested shape: arguments.edits[] of {oldText, newText}.
            if let Some(items) = args.get("edits").and_then(Value::as_array) {
                let edits = items
                    .iter()
                    .filter_map(|item| {
                        let old = item.get("oldText").and_then(Value::as_str)?;
                        let new = item.get("newText").and_then(Value::as_str)?;
                        Some(EditOp::new(old, new))
                    })
                    .collect();
                return Some((path, None, edits));
            }
            // Legacy flat shape: arguments.{oldText, newText}.
            let old = str_field("oldText")?;
            let new = str_field("newText").unwrap_or_default();
            Some((path, None, vec![EditOp::new(old, new)]))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::PiAdapter;
    use crate::models::Provider;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discovers_and_parses_pi_sessions() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let project = root.join("--Users-example-src-demo--");
        fs::create_dir_all(&project).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/example/src/demo"}
{"type":"model_change","id":"d33038ea","timestamp":"2026-06-18T17:31:17.989Z","provider":"anthropic","modelId":"claude"}
{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"Add pi support to aise"}]}}
{"type":"message","id":"79edf972","timestamp":"2026-06-18T17:31:36.595Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret reasoning"},{"type":"text","text":"I will wire up a pi adapter."},{"type":"toolCall","id":"t1","name":"ls","arguments":{"path":"/tmp"}}]}}
{"type":"message","id":"acb29b9d","timestamp":"2026-06-18T17:31:40.000Z","message":{"role":"toolResult","toolCallId":"t1","toolName":"ls","content":[{"type":"text","text":"Cargo.toml"}]}}
"#,
        )
        .unwrap();

        // Subagent transcript nested under the parent's session directory. Indexed as a
        // session of its own; `a_pi_subagent_run_keeps_its_own_id_and_names_its_parent`
        // covers what it parses to.
        let nested = project
            .join("2026-06-18T17-31-17-343Z_019edbc9-83df-72a0-a95b-64e6d810ad75")
            .join("agent01")
            .join("run-0");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("session.jsonl"),
            r#"{"type":"session","version":3,"id":"deadbeef-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/example/src/demo"}
"#,
        )
        .unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 2, "{sources:#?}");
        let top_level = sources
            .iter()
            .find(|source| source.path == transcript_path)
            .expect("the top-level transcript is discovered");
        assert_eq!(top_level.provider, Provider::Pi);

        let parsed = adapter.parse(top_level);
        assert_eq!(parsed.session.id, format!("pi:{session_id}"));
        assert_eq!(parsed.session.provider_session_id, session_id);
        assert_eq!(
            parsed.session.cwd.as_deref(),
            Some("/Users/example/src/demo")
        );
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Add pi support to aise")
        );
        // user + toolCall + assistant + toolResult.
        assert_eq!(parsed.session.message_count, Some(4));
        assert!(parsed.transcript_text.contains("Add pi support to aise"));
        assert!(parsed
            .transcript_text
            .contains("I will wire up a pi adapter."));
        // Thinking and tool payloads stay out of the transcript/title/preview.
        assert!(!parsed.transcript_text.contains("secret reasoning"));
        assert!(!parsed.transcript_text.contains("toolCall"));
        assert!(!parsed.transcript_text.contains("Cargo.toml"));
        // The toolResult is indexed as a tool message tagged with its tool name.
        let tool = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolResult)
            .expect("toolResult indexed as a Role::Tool message");
        assert_eq!(tool.tool_name.as_deref(), Some("ls"));
        assert_eq!(tool.content, "Cargo.toml");
        assert_eq!(tool.kind, crate::models::MessageKind::ToolResult);
        assert_eq!(tool.tool_call_id.as_deref(), Some("t1"));
        let call = parsed
            .messages
            .iter()
            .find(|message| message.kind == crate::models::MessageKind::ToolCall)
            .expect("toolCall input indexed as a tool-call message");
        assert_eq!(call.tool_name.as_deref(), Some("ls"));
        assert_eq!(call.tool_call_id.as_deref(), Some("t1"));
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Human
        );
        let identity = parsed.messages[0]
            .provenance
            .correlation_identity
            .as_ref()
            .expect("Pi message event id is retained");
        assert_eq!(
            identity.authority,
            crate::models::MessageCorrelationAuthority::Pi
        );
        assert_eq!(identity.scope, format!("pi:{session_id}"));
        assert_eq!(identity.id, "4abe1450");
        let assistant = parsed
            .messages
            .iter()
            .find(|message| message.content == "I will wire up a pi adapter.")
            .expect("assistant conversation row");
        assert_eq!(
            assistant
                .provenance
                .correlation_identity
                .as_ref()
                .map(|identity| identity.id.as_str()),
            Some("79edf972")
        );
        assert_eq!(
            tool.provenance
                .correlation_identity
                .as_ref()
                .map(|identity| identity.id.as_str()),
            Some("acb29b9d")
        );
        assert!(
            call.provenance.correlation_identity.is_none(),
            "tool-call correlation is not message-copy identity"
        );
    }

    /// Unlike claude, pi gives a spawned run a session id from the same space as a top-level
    /// one, so the id is used unchanged and only the link is added. Qualifying it with the
    /// parent would rename a session pi itself can address.
    #[test]
    fn a_pi_subagent_run_keeps_its_own_id_and_names_its_parent() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let parent_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let run_id = "deadbeef-83df-72a0-a95b-64e6d810ad75";
        let session_dir = root
            .join("--Users-example-src-demo--")
            .join(format!("2026-06-18T17-31-17-343Z_{parent_id}"));
        let run = session_dir.join("agent01").join("run-0");
        fs::create_dir_all(&run).unwrap();
        fs::write(
            run.join("session.jsonl"),
            format!(
                r#"{{"type":"session","version":3,"id":"{run_id}","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/example/src/demo"}}
{{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{{"role":"user","content":[{{"type":"text","text":"trace the caller"}}]}}}}
"#
            ),
        )
        .unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(
            sources.len(),
            1,
            "the nested run is a session: {sources:#?}"
        );
        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.provider_session_id, run_id);
        assert_eq!(
            parsed.session.parent_session_id.as_deref(),
            Some(format!("pi:{parent_id}").as_str())
        );
        assert_eq!(
            parsed.session.agent_label.as_deref(),
            Some("agent01"),
            "the agent directory is the only name pi records for the spawned agent"
        );
        assert!(parsed.transcript_text.contains("trace the caller"));
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
    }

    /// A run whose transcript names no id falls back to a parent-qualified name rather than a
    /// shared placeholder. Two runs both falling back to one id would upsert onto each other.
    #[test]
    fn a_pi_run_with_no_session_record_falls_back_to_a_parent_qualified_id() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let parent_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let session_dir = root
            .join("--Users-example-src-demo--")
            .join(format!("2026-06-18T17-31-17-343Z_{parent_id}"));
        for agent in ["agent01", "agent02"] {
            let run = session_dir.join(agent).join("run-0");
            fs::create_dir_all(&run).unwrap();
            fs::write(
                run.join("session.jsonl"),
                format!(
                    r#"{{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{{"role":"user","content":[{{"type":"text","text":"work by {agent}"}}]}}}}
"#
                ),
            )
            .unwrap();
        }

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 2, "{sources:#?}");
        let mut ids: Vec<String> = sources
            .iter()
            .map(|source| adapter.parse(source).session.provider_session_id)
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                format!("{parent_id}/agent01/run-0/session"),
                format!("{parent_id}/agent02/run-0/session"),
            ]
        );
    }

    #[test]
    fn extracts_file_edits_from_pi_write_and_edit() {
        use crate::models::EditOp;
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let project = root.join("--Users-example-src-demo--");
        fs::create_dir_all(&project).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        // Real pi shapes:
        //   write -> {path, content}
        //   edit  -> legacy flat {path, oldText, newText}
        //   edit  -> nested {path, edits:[{oldText, newText}, ...]}
        // A tool-only assistant turn (no text block) must still record its edit.
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/example/src/demo"}
{"type":"message","id":"m1","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"edit some files"}]}}
{"type":"message","id":"m2","timestamp":"2026-06-18T17:31:36.595Z","message":{"role":"assistant","content":[{"type":"text","text":"writing it"},{"type":"toolCall","id":"t1","name":"write","arguments":{"path":"src/new.ts","content":"export const x = 1;"}}]}}
{"type":"message","id":"r2","timestamp":"2026-06-18T17:31:38.000Z","message":{"role":"toolResult","toolCallId":"t1","toolName":"write","content":[{"type":"text","text":"written"}],"isError":false}}
{"type":"message","id":"m3","timestamp":"2026-06-18T17:31:40.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"t2","name":"edit","arguments":{"path":"src/legacy.ts","oldText":"import a","newText":"import b"}}]}}
{"type":"message","id":"r3","timestamp":"2026-06-18T17:31:42.000Z","message":{"role":"toolResult","toolCallId":"t2","toolName":"edit","content":[{"type":"text","text":"edited"}],"isError":false}}
{"type":"message","id":"m4","timestamp":"2026-06-18T17:31:44.000Z","message":{"role":"assistant","content":[{"type":"text","text":"and nested"},{"type":"toolCall","id":"t3","name":"edit","arguments":{"path":"src/nested.ts","edits":[{"oldText":"a","newText":"b"},{"oldText":"c","newText":"d"}]}}]}}
{"type":"message","id":"r4","timestamp":"2026-06-18T17:31:46.000Z","message":{"role":"toolResult","toolCallId":"t3","toolName":"edit","content":[{"type":"text","text":"edited"}],"isError":false}}
{"type":"message","id":"m5","timestamp":"2026-06-18T17:31:48.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"t4","name":"ls","arguments":{"path":"/tmp"}}]}}
"#,
        )
        .unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);

        // write + legacy edit + nested edit = 3 file edits; `ls` is not a mutation.
        assert_eq!(parsed.file_edits.len(), 3, "{:?}", parsed.file_edits);

        // write: full-content snapshot, replayable.
        let write = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "new.ts")
            .unwrap();
        assert_eq!(write.tool, "write");
        assert_eq!(write.file_path, "src/new.ts");
        assert_eq!(write.new_content.as_deref(), Some("export const x = 1;"));
        assert!(write.edits.is_empty());

        // legacy flat edit: one delta op, no full content.
        let legacy = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "legacy.ts")
            .unwrap();
        assert_eq!(legacy.tool, "edit");
        assert!(legacy.new_content.is_none());
        assert_eq!(legacy.edits, vec![EditOp::new("import a", "import b")]);

        // nested edit: two delta ops in order.
        let nested = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "nested.ts")
            .unwrap();
        assert_eq!(nested.tool, "edit");
        assert_eq!(
            nested.edits,
            vec![EditOp::new("a", "b"), EditOp::new("c", "d")]
        );
    }

    #[test]
    fn file_edits_commit_only_after_explicit_successful_tool_results() {
        let temp = tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let project = root.join("--Users-example-src-demo--");
        fs::create_dir_all(&project).unwrap();
        let session_id = "029edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("session_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"029edbc9-83df-72a0-a95b-64e6d810ad75","cwd":"/Users/example/src/demo"}
{"type":"message","id":"c1","message":{"role":"assistant","content":[{"type":"toolCall","id":"write-ok","name":"write","arguments":{"path":"src/a.ts","content":"alpha"}}]}}
{"type":"message","id":"r1","message":{"role":"toolResult","toolCallId":"write-ok","toolName":"write","content":[{"type":"text","text":"written"}],"isError":false}}
{"type":"message","id":"c2","message":{"role":"assistant","content":[{"type":"toolCall","id":"edit-ok","name":"edit","arguments":{"path":"src/a.ts","oldText":"alpha","newText":"beta"}}]}}
{"type":"message","id":"r2","message":{"role":"toolResult","toolCallId":"edit-ok","toolName":"edit","content":[{"type":"text","text":"edited"}],"isError":false}}
{"type":"message","id":"c3","message":{"role":"assistant","content":[{"type":"toolCall","id":"edit-failed","name":"edit","arguments":{"path":"src/a.ts","oldText":"beta","newText":"corrupt"}}]}}
{"type":"message","id":"r3","message":{"role":"toolResult","toolCallId":"edit-failed","toolName":"edit","content":[{"type":"text","text":"oldText not found"}],"isError":true}}
{"type":"message","id":"c4","message":{"role":"assistant","content":[{"type":"toolCall","id":"multi-ok","name":"edit","arguments":{"path":"src/a.ts","edits":[{"oldText":"beta","newText":"gamma"},{"oldText":"gamma","newText":"delta"}]}}]}}
{"type":"message","id":"r4","message":{"role":"toolResult","toolCallId":"multi-ok","toolName":"edit","content":[{"type":"text","text":"edited"}],"isError":false}}
{"type":"message","id":"c5","message":{"role":"assistant","content":[{"type":"toolCall","id":"write-later","name":"write","arguments":{"path":"src/a.ts","content":"snapshot"}}]}}
{"type":"message","id":"r5","message":{"role":"toolResult","toolCallId":"write-later","toolName":"write","content":[{"type":"text","text":"written"}],"isError":false}}
{"type":"message","id":"c6","message":{"role":"assistant","content":[{"type":"toolCall","id":"edit-unresolved","name":"edit","arguments":{"path":"src/a.ts","oldText":"snapshot","newText":"not observed"}}]}}
"#,
        )
        .unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert_eq!(
            parsed
                .file_edits
                .iter()
                .map(|edit| (edit.seq, edit.tool.as_str()))
                .collect::<Vec<_>>(),
            [(0, "write"), (1, "edit"), (2, "edit"), (3, "write")]
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

    #[test]
    fn falls_back_to_filename_id_when_session_line_has_no_id() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let project = root.join("--Users-example-src-demo--");
        fs::create_dir_all(&project).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/example/src/demo"}
{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"Add pi support to aise"}]}}
"#,
        )
        .unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);

        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.provider_session_id, session_id);
        assert_eq!(parsed.session.id, format!("pi:{session_id}"));
    }

    #[test]
    fn discovers_sessions_when_root_is_a_project_directory() {
        let temp = tempdir().expect("tempdir");
        // Root points directly at a single project's session dir, so transcripts
        // sit one level below the root instead of two.
        let root = temp.path().join("--Users-example-src-demo--");
        fs::create_dir_all(&root).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = root.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/example/src/demo"}
{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"Add pi support"}]}}
"#,
        )
        .unwrap();

        // A nested path that names no parent session directory is not a run of anything, so
        // it stays out. `agent01` is a directory name, not a session id.
        let nested = root.join("agent01").join("run-0");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("session.jsonl"), "{}\n").unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].path, transcript_path);
    }

    /// Differential guard for the streaming-parse refactor (task #241): identical output
    /// and `line_count` between the streaming `BufReader` path and the prior whole-file
    /// `fs::read_to_string` path. Fixture has a blank line, a malformed line, and a final
    /// line without a trailing newline (line_count counts all 5 physical lines).
    #[test]
    fn streaming_parse_output_is_stable() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("--Users-x-src-demo--");
        fs::create_dir_all(&root).unwrap();
        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = root.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        // 5 physical lines, no trailing newline on the last:
        //   1 session  2 user  3 malformed (skipped)  4 blank  5 assistant (no \n)
        let content = concat!(
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/x/src/demo"}"#,
            "\n",
            r#"{"type":"message","id":"m1","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"add pi support"}]}}"#,
            "\n",
            "{bad json\n",
            "\n",
            r#"{"type":"message","id":"m2","timestamp":"2026-06-18T17:31:36.595Z","message":{"role":"assistant","content":[{"type":"text","text":"will do"}]}}"#,
        );
        fs::write(&transcript_path, content).unwrap();

        let adapter = PiAdapter::new(vec![root]);
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
        assert_eq!(parsed.session.provider, Provider::Pi);
        assert_eq!(parsed.session.cwd.as_deref(), Some("/Users/x/src/demo"));
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["add pi support", "will do"]);
        let roles: Vec<&str> = parsed.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant"]);
        assert!(parsed.transcript_text.contains("add pi support"));
        assert!(parsed.transcript_text.contains("will do"));
        assert_eq!(
            parsed.session.parse_warning.as_deref(),
            Some("skipped 1 malformed JSONL record")
        );
    }

    /// Non-UTF-8 bytes must never panic or abort the parse — they are decoded lossily (U+FFFD).
    /// This input is not valid JSON even after lossy decoding, so it yields no messages, but
    /// parsing completes WITHOUT error (lossy recovery is not treated as a parse failure).
    #[test]
    fn non_utf8_garbage_parses_gracefully_without_error() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("--Users-x-src-demo--");
        fs::create_dir_all(&root).unwrap();
        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = root.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(&transcript_path, [b'{', 0xFF, 0xFE, b'}', b'\n']).unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        let parsed = adapter.parse(&sources[0]);
        assert!(parsed.messages.is_empty());
        assert!(
            parsed.session.parse_warning.is_none(),
            "lossy recovery is not an error, so no parse warning is set"
        );
        assert_eq!(parsed.session.message_count, Some(0));
    }
}
