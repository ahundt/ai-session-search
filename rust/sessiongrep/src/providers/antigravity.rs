use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, preview_from_text, substantive_text, truncate_for_display,
};

pub struct AntigravityAdapter {
    roots: Vec<PathBuf>,
}

impl AntigravityAdapter {
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
                if path.file_name().and_then(|n| n.to_str()) != Some("transcript.jsonl") {
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
                        provider: Provider::Antigravity,
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
            Err(err) => minimal_record(Provider::Antigravity, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)?;
        
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
        let mut raw_meta = Vec::new();
        let mut last_prompt = None;

        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            raw_meta.push(value.clone());

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

            let source = value.get("source").and_then(Value::as_str).unwrap_or("");
            let text = value.get("content").and_then(Value::as_str).unwrap_or("").trim().to_string();

            if text.is_empty() {
                continue;
            }

            let role = match source {
                "USER_EXPLICIT" | "USER" => Some("user"),
                "MODEL" => Some("assistant"),
                _ => None,
            };

            if let Some(role) = role {
                if role == "user" && substantive_text(&text) {
                    last_prompt = Some(text.clone());
                }
                messages.push((role.to_string(), text.clone(), timestamp));
                transcript_lines.push(format_transcript_line(role, timestamp, &text));
            }
        }

        let first_user = messages
            .iter()
            .find(|(role, text, _)| role == "user" && substantive_text(text))
            .map(|(_, text, _)| text.clone());
        let last_user = messages
            .iter()
            .rev()
            .find(|(role, text, _)| role == "user" && substantive_text(text))
            .map(|(_, text, _)| text.clone());
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
            "line_count": raw.lines().count(),
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
        })
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
        let session_dir = dir.path().join("94fc19cc-ad62-42eb-aef9-c43deed34236/.system_generated/logs");
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
        assert_eq!(parsed.session.provider_session_id, "94fc19cc-ad62-42eb-aef9-c43deed34236");
        assert_eq!(parsed.session.id, "antigravity:94fc19cc-ad62-42eb-aef9-c43deed34236");
        assert_eq!(parsed.session.cwd.as_deref(), Some("/path/to/repo"));
        assert_eq!(parsed.session.message_count, Some(2));
        assert!(parsed.transcript_text.contains("hello agent"));
        assert!(parsed.transcript_text.contains("hello user"));
    }
}
