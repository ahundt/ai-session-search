use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{json, Value};

use crate::models::{FileEdit, ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    find_repo_root, format_transcript_line, minimal_record, normalize_path, parse_datetime,
    preview_from_text, substantive_text, truncate_for_display,
};

pub struct AntigravityAdapter {
    roots: Vec<PathBuf>,
}

impl AntigravityAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        let mut files_by_log_dir: BTreeMap<PathBuf, SourceFile> = BTreeMap::new();
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
                if antigravity_transcript_priority(path) == 0 {
                    continue;
                }
                if let Ok(metadata) = entry.metadata() {
                    let mtime_ns = metadata
                        .modified()
                        .ok()
                        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|value| value.as_nanos() as i64)
                        .unwrap_or_default();
                    let source = SourceFile {
                        provider: Provider::Antigravity,
                        path: path.to_path_buf(),
                        mtime_ns,
                        size_bytes: metadata.len() as i64,
                    };
                    let log_dir = path.parent().unwrap_or(path).to_path_buf();
                    let replace = files_by_log_dir
                        .get(&log_dir)
                        .map(|existing| {
                            antigravity_transcript_priority(&existing.path)
                                < antigravity_transcript_priority(&source.path)
                        })
                        .unwrap_or(true);
                    if replace {
                        files_by_log_dir.insert(log_dir, source);
                    }
                }
            }
        }
        files_by_log_dir.into_values().collect()
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        match self.parse_inner(&source.path) {
            Ok(parsed) => parsed,
            Err(err) => minimal_record(Provider::Antigravity, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let file = std::fs::File::open(path)?;
        self.parse_reader(std::io::BufReader::new(file), path)
    }

    /// Parse antigravity session lines from any reader. `parse_inner` calls this over the file;
    /// the incremental tail parser ([`crate::tail`]) calls it over an in-memory byte slice of the
    /// appended region, so the per-line logic lives in ONE place (a differential test asserts a
    /// tail parse equals a full parse). Streams line-by-line (task #241); `line_count` is tallied
    /// in this single pass. See claude::parse_reader notes.
    pub fn parse_reader<R: std::io::BufRead>(
        &self,
        reader: R,
        path: &Path,
    ) -> Result<ParsedSession> {
        let mut line_count: usize = 0;

        // Extract session ID from path. The path structure is:
        // .../brain/<conversation-id>/.system_generated/logs/transcript.jsonl
        // So we traverse up 3 times to get the conversation ID directory name.
        let provider_session_id = path
            .parent() // .../brain/<conversation-id>/.system_generated/logs
            .and_then(|p| p.parent()) // .../brain/<conversation-id>/.system_generated
            .and_then(|p| p.parent()) // .../brain/<conversation-id>
            .and_then(|p| p.file_name())
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();

        let mut cwd = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();
        let mut last_prompt = None;
        let mut file_edits: Vec<FileEdit> = Vec::new();
        let mut file_edit_seq: i64 = 0;

        for line in crate::util::lines_replacing_invalid_utf8(reader) {
            let line = line?;
            line_count += 1;
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let timestamp = value
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(parse_datetime);

            if created_at.is_none() && timestamp.is_some() {
                created_at = timestamp;
            }
            if timestamp.is_some() {
                updated_at = timestamp;
            }

            // Extract cwd from tool_calls args
            if cwd.is_none() {
                if let Some(tool_calls) = value.get("tool_calls").and_then(Value::as_array) {
                    for call in tool_calls {
                        if let Some(args) = call.get("args") {
                            if let Some(c) = args.get("Cwd").and_then(Value::as_str) {
                                cwd = Some(c.to_string());
                                break;
                            }
                            if let Some(c) = args.get("cwd").and_then(Value::as_str) {
                                cwd = Some(c.to_string());
                                break;
                            }
                        }
                    }
                }
            }

            // Extract file-mutating tool_calls before the empty-content skip — a
            // file-change/tool step can carry tool_calls with no `content`.
            collect_antigravity_file_edits(&value, timestamp, &mut file_edit_seq, &mut file_edits);

            let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
            let source = value.get("source").and_then(Value::as_str).unwrap_or("");
            // Check emptiness on the borrow before allocating, so records skipped for being
            // empty (or not a real turn) never pay the trimmed-`String` allocation.
            let text = value.get("content").and_then(Value::as_str).unwrap_or("");
            if text.trim().is_empty() {
                continue;
            }

            let Some((role, tool_name)) = classify_antigravity_record(record_type, source) else {
                continue;
            };
            let text = text.trim().to_string();

            if role == "user" && substantive_text(&text) {
                last_prompt = Some(text.clone());
            }
            messages.push((role.to_string(), text.clone(), timestamp, tool_name));
            // Tool-step output stays out of the human transcript/title/preview, matching how
            // claude/codex/pi keep tool results separate from the conversation.
            if role != "tool" {
                transcript_lines.push(format_transcript_line(role, timestamp, &text));
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
        let title = last_prompt
            .clone()
            .or_else(|| last_user.clone())
            .or_else(|| first_user.clone())
            .clone()
            .map(|text| truncate_for_display(&text, 100));
        let preview = last_prompt
            .clone()
            .or_else(|| last_user.clone())
            .or_else(|| first_user.clone())
            .map(|text| preview_from_text(&text))
            .unwrap_or_else(|| "(no preview available)".to_string());

        let repo_root = cwd.as_deref().and_then(find_repo_root);
        let raw_metadata_json = Some(serde_json::to_string(&json!({
            "line_count": line_count,
            "session_path": normalize_path(path),
        }))?);

        let session = SessionRecord {
            id: format!("antigravity:{provider_session_id}"),
            provider: Provider::Antigravity,
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
            parse_version: "antigravity-v1".to_string(),
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
}

fn antigravity_transcript_priority(path: &Path) -> u8 {
    match path.file_name().and_then(|name| name.to_str()) {
        Some("transcript_full.jsonl") => 2,
        Some("transcript.jsonl") => 1,
        _ => 0,
    }
}

/// Classify one flat-transcript record into `(role, tool_name)`, or `None` to skip it.
///
/// Antigravity's flat transcript JSONL distinguishes turns by `type`: `USER_INPUT`
/// (the user prompt), `PLANNER_RESPONSE` (the assistant), and a family of tool-step types
/// (`RUN_COMMAND`, `SEARCH_WEB`, `VIEW_FILE`, `CODE_ACTION`, `FILE_CHANGE`, `GREP_SEARCH`,
/// `LIST_DIRECTORY`, …) whose `content` holds that tool's output. `CONVERSATION_HISTORY`
/// replays earlier turns, so it is skipped to avoid duplicating real messages. A tool step
/// becomes a [`Role::Tool`](crate::models::Role) message tagged with the lower-cased step
/// type as its tool name. Shapes verified from the Tier-1 real dump (Medium "Antigravity
/// CLI Tutorial Series Part 2") cross-checked against the kenn-io/agentsview parser.
fn classify_antigravity_record(
    record_type: &str,
    source: &str,
) -> Option<(&'static str, Option<String>)> {
    match record_type {
        "USER_INPUT" => Some(("user", None)),
        "PLANNER_RESPONSE" => Some(("assistant", None)),
        "CONVERSATION_HISTORY" => None,
        // No `type`: fall back to the `source` field (older/partial records).
        "" => match source {
            "USER_EXPLICIT" | "USER" => Some(("user", None)),
            "MODEL" => Some(("assistant", None)),
            _ => None,
        },
        // Any other typed record is a tool step. A user-sourced one (rare) stays `user`;
        // everything else is tool output tagged with the step type.
        other => match source {
            "USER_EXPLICIT" | "USER" => Some(("user", None)),
            _ => Some(("tool", Some(other.to_ascii_lowercase()))),
        },
    }
}

/// Append a path-only [`FileEdit`] for each file-mutating tool_call on a record.
///
/// Antigravity's edit tools are `write_to_file`, `replace_file_content`, and
/// `multi_replace_file_content` — names verified from the extracted system prompt and the
/// kenn-io/agentsview parser (which reads `TargetFile`/`file_path`/`path`). The arg
/// *casing/content* as serialized into the flat transcript is unverified (no real dump
/// shows a populated edit tool_call — the write tool's fields live in an opaque embedded
/// struct), so this records **path-only**: the edit appears in `files search`/`history`/
/// `cross-ref` but is not reconstructable via `files extract`. Reading the three known key
/// spellings makes it a graceful no-op when the field is absent rather than emitting wrong
/// data.
fn collect_antigravity_file_edits(
    record: &Value,
    ts: Option<DateTime<Utc>>,
    next_seq: &mut i64,
    out: &mut Vec<FileEdit>,
) {
    let Some(tool_calls) = record.get("tool_calls").and_then(Value::as_array) else {
        return;
    };
    for call in tool_calls {
        let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
        if !matches!(
            name,
            "write_to_file" | "replace_file_content" | "multi_replace_file_content"
        ) {
            continue;
        }
        let Some(args) = call.get("args") else {
            continue;
        };
        let Some(file_path) = ["TargetFile", "file_path", "path"]
            .iter()
            .find_map(|key| args.get(*key).and_then(Value::as_str))
            .map(str::to_string)
        else {
            continue;
        };
        let file_name = crate::util::file_basename(&file_path);
        out.push(FileEdit {
            seq: *next_seq,
            ts,
            tool: name.to_string(),
            file_path,
            file_name,
            new_content: None,
            edits: Vec::new(),
        });
        *next_seq += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_antigravity_parser() {
        let dir = tempdir().unwrap();
        let session_dir = dir
            .path()
            .join("94fc19cc-ad62-42eb-aef9-c43deed34236/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        let log_file = session_dir.join("transcript.jsonl");

        let log_content = r#"
{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-19T23:13:00Z","content":"hello agent"}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-19T23:13:05Z","content":"hello user","tool_calls":[{"name":"run_command","args":{"Cwd":"/path/to/repo"}}]}
"#;
        fs::write(&log_file, log_content.trim()).unwrap();

        let adapter = AntigravityAdapter::new(vec![dir.path().to_path_buf()]);
        let files = adapter.discover();
        assert_eq!(files.len(), 1);

        let parsed = adapter.parse(&files[0]);
        assert_eq!(
            parsed.session.provider_session_id,
            "94fc19cc-ad62-42eb-aef9-c43deed34236"
        );
        assert_eq!(
            parsed.session.id,
            "antigravity:94fc19cc-ad62-42eb-aef9-c43deed34236"
        );
        assert_eq!(parsed.session.cwd.as_deref(), Some("/path/to/repo"));
        assert_eq!(parsed.session.message_count, Some(2));
        assert!(parsed.transcript_text.contains("hello agent"));
        assert!(parsed.transcript_text.contains("hello user"));
    }

    #[test]
    fn discovers_antigravity_cli_brain_transcripts() {
        let dir = tempdir().unwrap();
        let root = dir.path().join(".gemini/antigravity-cli/brain");
        let session_dir = root.join("5976941f-b237-4440-a02c-39593889400c/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        let log_file = session_dir.join("transcript.jsonl");

        let log_content = r#"
{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-06-30T06:19:23Z","content":"<USER_REQUEST>\ntesting antigravity\n</USER_REQUEST>"}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-06-30T06:19:23Z","content":"I am going to inspect the guide.","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"/Users/alice/.gemini/antigravity-cli/builtin/skills/antigravity_guide/SKILL.md","toolSummary":"View Antigravity Guide SKILL.md"}}]}
{"step_index":3,"source":"MODEL","type":"VIEW_FILE","status":"DONE","created_at":"2026-06-30T06:19:24Z","content":"File Path: `file:///Users/alice/.gemini/antigravity-cli/builtin/skills/antigravity_guide/SKILL.md`\nTotal Lines: 54"}
"#;
        fs::write(&log_file, log_content.trim()).unwrap();

        let adapter = AntigravityAdapter::new(vec![root]);
        let files = adapter.discover();
        assert_eq!(files.len(), 1);

        let parsed = adapter.parse(&files[0]);
        assert_eq!(
            parsed.session.provider_session_id,
            "5976941f-b237-4440-a02c-39593889400c"
        );
        assert_eq!(parsed.session.message_count, Some(3));
        assert_eq!(parsed.messages[0].role.as_str(), "user");
        assert!(parsed.messages[0].content.contains("testing antigravity"));
        let tool = parsed
            .messages
            .iter()
            .find(|message| message.tool_name.as_deref() == Some("view_file"))
            .expect("VIEW_FILE step indexed as a tool message");
        assert!(tool.content.contains("Total Lines: 54"));
    }

    #[test]
    fn discover_prefers_full_transcript_when_both_exist() {
        let dir = tempdir().unwrap();
        let root = dir.path().join(".gemini/antigravity-cli/brain");
        let session_dir = root.join("769b6413-3652-42c0-a7c2-3c427cc99264/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("transcript.jsonl"), "{}\n").unwrap();
        fs::write(
            session_dir.join("transcript_full.jsonl"),
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-06-30T06:49:05Z","content":"<USER_REQUEST>\nfull transcript\n</USER_REQUEST>"}"#,
        )
        .unwrap();

        let adapter = AntigravityAdapter::new(vec![root]);
        let files = adapter.discover();
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0].path.file_name().and_then(|name| name.to_str()),
            Some("transcript_full.jsonl")
        );
        let parsed = adapter.parse(&files[0]);
        assert_eq!(
            parsed.session.provider_session_id,
            "769b6413-3652-42c0-a7c2-3c427cc99264"
        );
        assert!(parsed.transcript_text.contains("full transcript"));
    }

    #[test]
    fn indexes_tool_step_records_as_tool_messages() {
        use crate::models::Role;
        let dir = tempdir().unwrap();
        let session_dir = dir
            .path()
            .join("94fc19cc-ad62-42eb-aef9-c43deed34236/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        let log_file = session_dir.join("transcript.jsonl");

        // Real flat-transcript shapes: a tool step (RUN_COMMAND) is its own record whose
        // `content` holds the tool output; CONVERSATION_HISTORY replays earlier turns and
        // must be skipped (not duplicated into the index).
        let log_content = r#"
{"step_index":0,"source":"MODEL","type":"CONVERSATION_HISTORY","status":"DONE","created_at":"2026-05-19T23:12:00Z","content":"earlier replayed turn"}
{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-19T23:13:00Z","content":"run ls"}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-19T23:13:02Z","content":"I'll run it","tool_calls":[{"name":"run_command","args":{"Cwd":"/repo"}}]}
{"step_index":3,"source":"MODEL","type":"RUN_COMMAND","status":"DONE","created_at":"2026-05-19T23:13:05Z","content":"file1.txt\nfile2.txt"}
"#;
        fs::write(&log_file, log_content.trim()).unwrap();

        let adapter = AntigravityAdapter::new(vec![dir.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        // user + assistant + the RUN_COMMAND tool step (CONVERSATION_HISTORY is skipped).
        assert_eq!(parsed.session.message_count, Some(3));
        let tool = parsed
            .messages
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("RUN_COMMAND indexed as a Role::Tool message");
        assert_eq!(tool.tool_name.as_deref(), Some("run_command"));
        assert_eq!(tool.content, "file1.txt\nfile2.txt");
        // Tool output and replayed history stay out of the human transcript.
        assert!(!parsed.transcript_text.contains("file1.txt"));
        assert!(!parsed.transcript_text.contains("earlier replayed turn"));
        // The replayed history is not indexed as any message either.
        assert!(!parsed
            .messages
            .iter()
            .any(|m| m.content.contains("earlier replayed turn")));
    }

    #[test]
    fn extracts_path_only_file_edits_from_tool_calls() {
        let dir = tempdir().unwrap();
        let session_dir = dir
            .path()
            .join("94fc19cc-ad62-42eb-aef9-c43deed34236/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        let log_file = session_dir.join("transcript.jsonl");

        // A PLANNER_RESPONSE issuing the two main edit tools. `args` casing is unverified
        // upstream, so extraction reads TargetFile/file_path/path and is path-only.
        let log_content = r#"
{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-19T23:13:00Z","content":"edit files"}
{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-19T23:13:02Z","content":"editing","tool_calls":[{"name":"write_to_file","args":{"TargetFile":"/repo/src/main.rs","CodeContent":"fn main(){}"}},{"name":"replace_file_content","args":{"TargetFile":"/repo/src/lib.rs"}},{"name":"run_command","args":{"CommandLine":"cargo build"}}]}
"#;
        fs::write(&log_file, log_content.trim()).unwrap();

        let adapter = AntigravityAdapter::new(vec![dir.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        // The two edit tools are recorded; run_command is not a file mutation.
        assert_eq!(parsed.file_edits.len(), 2, "{:?}", parsed.file_edits);
        let names: Vec<&str> = parsed
            .file_edits
            .iter()
            .map(|e| e.file_name.as_str())
            .collect();
        assert!(names.contains(&"main.rs"), "{names:?}");
        assert!(names.contains(&"lib.rs"), "{names:?}");
        // Path-only: no replayable content captured (casing/content unverified upstream).
        assert!(parsed
            .file_edits
            .iter()
            .all(|e| e.new_content.is_none() && e.edits.is_empty()));
        let main = parsed
            .file_edits
            .iter()
            .find(|e| e.file_name == "main.rs")
            .unwrap();
        assert_eq!(main.tool, "write_to_file");
        assert_eq!(main.file_path, "/repo/src/main.rs");
    }

    /// Differential guard for the streaming-parse refactor (task #241): identical output
    /// and `line_count` between the streaming `BufReader` path and the prior whole-file
    /// `fs::read_to_string` path. Fixture has a leading blank line, a malformed line, and a
    /// final line without a trailing newline (line_count counts all 4 physical lines).
    #[test]
    fn streaming_parse_output_is_stable() {
        use crate::models::Role;
        let dir = tempdir().unwrap();
        let session_dir = dir
            .path()
            .join("94fc19cc-ad62-42eb-aef9-c43deed34236/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        let log_file = session_dir.join("transcript.jsonl");
        // 4 physical lines, no trailing newline on the last:
        //   1 blank  2 user  3 malformed (skipped)  4 planner response (no \n)
        let content = concat!(
            "\n",
            r#"{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-05-19T23:13:00Z","content":"hello agent"}"#,
            "\n",
            "{bad json\n",
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE","created_at":"2026-05-19T23:13:05Z","content":"hello user","tool_calls":[{"name":"run_command","args":{"Cwd":"/path/to/repo"}}]}"#,
        );
        fs::write(&log_file, content).unwrap();

        let adapter = AntigravityAdapter::new(vec![dir.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);

        assert!(
            parsed
                .session
                .raw_metadata_json
                .as_deref()
                .unwrap()
                .contains("\"line_count\":4"),
            "line_count must be 4, got: {:?}",
            parsed.session.raw_metadata_json
        );
        assert_eq!(parsed.session.cwd.as_deref(), Some("/path/to/repo"));
        let contents: Vec<&str> = parsed.messages.iter().map(|m| m.content.as_str()).collect();
        assert_eq!(contents, vec!["hello agent", "hello user"]);
        let roles: Vec<Role> = parsed.messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
        assert!(parsed.transcript_text.contains("hello agent"));
        assert!(parsed.transcript_text.contains("hello user"));
    }

    /// Non-UTF-8 bytes must never panic or abort the parse — they are decoded lossily (U+FFFD).
    /// This input is not valid JSON even after lossy decoding, so it yields no messages, but
    /// parsing completes WITHOUT error (lossy recovery is not treated as a parse failure).
    #[test]
    fn non_utf8_garbage_parses_gracefully_without_error() {
        let dir = tempdir().unwrap();
        let session_dir = dir
            .path()
            .join("94fc19cc-ad62-42eb-aef9-c43deed34236/.system_generated/logs");
        fs::create_dir_all(&session_dir).unwrap();
        let log_file = session_dir.join("transcript.jsonl");
        fs::write(&log_file, [b'{', 0xFF, 0xFE, b'}', b'\n']).unwrap();

        let adapter = AntigravityAdapter::new(vec![dir.path().to_path_buf()]);
        let parsed = adapter.parse(&adapter.discover()[0]);
        assert!(parsed.messages.is_empty());
        assert!(
            parsed.session.parse_warning.is_none(),
            "lossy recovery is not an error, so no parse warning is set"
        );
        assert_eq!(parsed.session.message_count, Some(0));
    }
}
