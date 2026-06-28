use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{json, Value};

use crate::models::{EditOp, FileEdit, ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, preview_from_text, substantive_text, truncate_for_display,
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
                // Top-level project sessions live directly under <root>/<encoded-cwd>/<file>.jsonl.
                // Subagent transcripts are nested deeper (<encoded-cwd>/<session>/<agent>/run-N/
                // session.jsonl); skip them to avoid duplicate, low-signal records.
                if !is_top_level_session(root, path) {
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
                        provider: Provider::Pi,
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
            Err(err) => minimal_record(Provider::Pi, &source.path, err.to_string()),
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
        let mut provider_session_id = self
            .extract_id(path)
            .unwrap_or_else(|| "unknown".to_string());
        let mut cwd = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut file_edit_seq: i64 = 0;

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
                Some("session") => {
                    if let Some(id) = value.get("id").and_then(Value::as_str) {
                        provider_session_id = id.to_string();
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
                        collect_pi_file_edits(
                            message,
                            timestamp,
                            &mut file_edit_seq,
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
                            messages.push((
                                role.unwrap_or("message").to_string(),
                                text.to_string(),
                                timestamp,
                                None,
                            ));
                            transcript_lines.push(format_transcript_line(
                                role.unwrap_or("message"),
                                timestamp,
                                text,
                            ));
                        }
                        // Tool output: index as a Role::Tool message (searchable via
                        // `messages search --type tool`), tagged with the tool's name, but
                        // kept out of the conversation transcript/title/preview.
                        Some("toolResult") => {
                            updated_at = timestamp.or(updated_at);
                            let tool_name = message
                                .get("toolName")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned);
                            messages.push((
                                "tool".to_string(),
                                text.to_string(),
                                timestamp,
                                tool_name,
                            ));
                        }
                        _ => continue,
                    }
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
        let raw_metadata_json = Some(serde_json::to_string(&json!({
            "line_count": line_count,
            "session_path": normalize_path(path),
        }))?);

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
            parse_version: "pi-v1".to_string(),
            raw_metadata_json,
            parse_warning: None,
            discovery_source: "jsonl".to_string(),
        };

        Ok(ParsedSession {
            session,
            transcript_text: transcript_lines.join("\n\n"),
            messages: crate::util::to_messages_with_tools(messages),
            file_edits,
        })
    }

    fn extract_id(&self, path: &Path) -> Option<String> {
        let stem = path.file_stem().and_then(|stem| stem.to_str())?;
        self.id_re
            .captures(stem)
            .and_then(|captures| captures.get(1))
            .map(|match_| match_.as_str().to_string())
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
/// sequence numbers. Verified against the pi reference dump (earendil-works/pi
/// `packages/coding-agent/test/fixtures/large-session.jsonl`: 146 `edit` + 3 `write`
/// real toolCalls). The two file-mutating tools are the only ones in pi's built-in set
/// (`read|bash|edit|write|grep|find|ls`); everything else is skipped.
fn collect_pi_file_edits(
    message: &Value,
    ts: Option<DateTime<Utc>>,
    next_seq: &mut i64,
    out: &mut Vec<FileEdit>,
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
        if let Some((file_path, new_content, edits)) =
            pi_tool_edit_payload(name, block.get("arguments"))
        {
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

/// Map a single pi `write`/`edit` toolCall to `(file_path, full_content?, edits)`.
/// `write` yields a full-content snapshot (replayable via `files extract`); `edit` yields
/// `old`→`new` delta ops. Pi's `edit` arguments appear in TWO shapes in the wild and BOTH
/// must be accepted (confirmed by pi's own `edit-tool-legacy-input.test.ts`):
///   - legacy flat: `{path, oldText, newText}` (what the reference dump persists)
///   - current nested: `{path, edits: [{oldText, newText}, ...]}`
fn pi_tool_edit_payload(
    name: &str,
    args: Option<&Value>,
) -> Option<(String, Option<String>, Vec<EditOp>)> {
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
        let project = root.join("--Users-nisarg-src-demo--");
        fs::create_dir_all(&project).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/nisarg/src/demo"}
{"type":"model_change","id":"d33038ea","timestamp":"2026-06-18T17:31:17.989Z","provider":"anthropic","modelId":"claude"}
{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"Add pi support to sessiongrep"}]}}
{"type":"message","id":"79edf972","timestamp":"2026-06-18T17:31:36.595Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret reasoning"},{"type":"text","text":"I will wire up a pi adapter."},{"type":"toolCall","id":"t1","name":"ls","arguments":{"path":"/tmp"}}]}}
{"type":"message","id":"acb29b9d","timestamp":"2026-06-18T17:31:40.000Z","message":{"role":"toolResult","toolCallId":"t1","toolName":"ls","content":[{"type":"text","text":"Cargo.toml"}]}}
"#,
        )
        .unwrap();

        // Subagent transcript nested deeper — must be ignored.
        let nested = project
            .join("2026-06-18T17-31-17-343Z_019edbc9-83df-72a0-a95b-64e6d810ad75")
            .join("agent01")
            .join("run-0");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("session.jsonl"),
            r#"{"type":"session","version":3,"id":"deadbeef-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/nisarg/src/demo"}
"#,
        )
        .unwrap();

        let adapter = PiAdapter::new(vec![root]);
        let sources = adapter.discover();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].provider, Provider::Pi);
        assert_eq!(sources[0].path, transcript_path);

        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.id, format!("pi:{session_id}"));
        assert_eq!(parsed.session.provider_session_id, session_id);
        assert_eq!(
            parsed.session.cwd.as_deref(),
            Some("/Users/nisarg/src/demo")
        );
        assert_eq!(
            parsed.session.title.as_deref(),
            Some("Add pi support to sessiongrep")
        );
        // user + assistant + the toolResult (now indexed as a Role::Tool message).
        assert_eq!(parsed.session.message_count, Some(3));
        assert!(parsed
            .transcript_text
            .contains("Add pi support to sessiongrep"));
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
            .find(|m| m.role == crate::models::Role::Tool)
            .expect("toolResult indexed as a Role::Tool message");
        assert_eq!(tool.tool_name.as_deref(), Some("ls"));
        assert_eq!(tool.content, "Cargo.toml");
    }

    #[test]
    fn extracts_file_edits_from_pi_write_and_edit() {
        use crate::models::EditOp;
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let project = root.join("--Users-nisarg-src-demo--");
        fs::create_dir_all(&project).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        // Real pi shapes (earendil-works/pi large-session.jsonl):
        //   write -> {path, content}
        //   edit  -> legacy flat {path, oldText, newText}
        //   edit  -> nested {path, edits:[{oldText, newText}, ...]}
        // A tool-only assistant turn (no text block) must still record its edit.
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/nisarg/src/demo"}
{"type":"message","id":"m1","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"edit some files"}]}}
{"type":"message","id":"m2","timestamp":"2026-06-18T17:31:36.595Z","message":{"role":"assistant","content":[{"type":"text","text":"writing it"},{"type":"toolCall","id":"t1","name":"write","arguments":{"path":"src/new.ts","content":"export const x = 1;"}}]}}
{"type":"message","id":"m3","timestamp":"2026-06-18T17:31:40.000Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"t2","name":"edit","arguments":{"path":"src/legacy.ts","oldText":"import a","newText":"import b"}}]}}
{"type":"message","id":"m4","timestamp":"2026-06-18T17:31:44.000Z","message":{"role":"assistant","content":[{"type":"text","text":"and nested"},{"type":"toolCall","id":"t3","name":"edit","arguments":{"path":"src/nested.ts","edits":[{"oldText":"a","newText":"b"},{"oldText":"c","newText":"d"}]}}]}}
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
    fn falls_back_to_filename_id_when_session_line_has_no_id() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().to_path_buf();
        let project = root.join("--Users-nisarg-src-demo--");
        fs::create_dir_all(&project).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = project.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/nisarg/src/demo"}
{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"Add pi support to sessiongrep"}]}}
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
        let root = temp.path().join("--Users-nisarg-src-demo--");
        fs::create_dir_all(&root).unwrap();

        let session_id = "019edbc9-83df-72a0-a95b-64e6d810ad75";
        let transcript_path = root.join(format!("2026-06-18T17-31-17-343Z_{session_id}.jsonl"));
        fs::write(
            &transcript_path,
            r#"{"type":"session","version":3,"id":"019edbc9-83df-72a0-a95b-64e6d810ad75","timestamp":"2026-06-18T17:31:17.343Z","cwd":"/Users/nisarg/src/demo"}
{"type":"message","id":"4abe1450","timestamp":"2026-06-18T17:31:32.922Z","message":{"role":"user","content":[{"type":"text","text":"Add pi support"}]}}
"#,
        )
        .unwrap();

        // Subagent transcript nested below the project root — still excluded.
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
