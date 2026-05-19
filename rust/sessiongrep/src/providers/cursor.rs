use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    find_repo_root, format_transcript_line, minimal_record, normalize_path, preview_from_text,
    substantive_text, truncate_for_display,
};

pub struct CursorAdapter {
    roots: Vec<PathBuf>,
}

impl CursorAdapter {
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
                    .any(|component| component.as_os_str() == "subagents")
                {
                    continue;
                }
                if !path
                    .components()
                    .any(|component| component.as_os_str() == "agent-transcripts")
                {
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
                        provider: Provider::Cursor,
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
            Err(err) => minimal_record(Provider::Cursor, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)?;
        let provider_session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("unknown")
            .to_string();
        let cwd = infer_cursor_workspace(path);
        let mut created_at: Option<DateTime<Utc>> = None;
        let updated_at = file_modified_at(path);
        let mut messages = Vec::new();
        let mut transcript_lines = Vec::new();

        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let Some(role) = value.get("role").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(role, "user" | "assistant") {
                continue;
            }
            let text = cursor_message_text(&value);
            if !substantive_text(&text) {
                continue;
            }
            if created_at.is_none() {
                created_at = updated_at;
            }
            messages.push((role.to_string(), text.clone()));
            transcript_lines.push(format_transcript_line(role, updated_at, &text));
        }

        let first_user = messages
            .iter()
            .find(|(role, text)| role == "user" && substantive_text(text))
            .map(|(_, text)| text.clone());
        let last_user = messages
            .iter()
            .rev()
            .find(|(role, text)| role == "user" && substantive_text(text))
            .map(|(_, text)| text.clone());
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
        let raw_metadata_json = Some(serde_json::to_string(&json!({
            "line_count": raw.lines().count(),
            "session_path": normalize_path(path),
        }))?);

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
            parse_version: "cursor-v1".to_string(),
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

fn cursor_message_text(value: &Value) -> String {
    let Some(message) = value.get("message") else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(content) = message.get("content") {
        collect_text_content(content, &mut parts);
    }
    let text = parts.join("\n");
    extract_tag(&text, "user_query")
        .unwrap_or(text)
        .trim()
        .to_string()
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

fn extract_tag(text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    Some(text[start..end].trim().to_string())
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

fn decode_cursor_project_dir(encoded: &str) -> Option<PathBuf> {
    if encoded == "empty-window" {
        return None;
    }
    let parts: Vec<&str> = encoded.split('-').collect();
    if parts.first().copied() != Some("Users") || parts.len() < 3 {
        return None;
    }
    let suffixes = partition_suffixes(&parts[1..]);
    suffixes
        .into_iter()
        .map(|suffix| {
            let mut path = PathBuf::from("/");
            path.push("Users");
            for part in suffix {
                path.push(part);
            }
            path
        })
        .find(|candidate| candidate.exists())
}

fn partition_suffixes(parts: &[&str]) -> Vec<Vec<String>> {
    fn walk(parts: &[&str], index: usize, current: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
        if index >= parts.len() {
            out.push(current.clone());
            return;
        }
        for end in index + 1..=parts.len() {
            current.push(parts[index..end].join("-"));
            walk(parts, end, current, out);
            current.pop();
        }
    }

    let mut out = Vec::new();
    walk(parts, 0, &mut Vec::new(), &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::{CursorAdapter, cursor_message_text, decode_cursor_project_dir, extract_tag};
    use crate::models::Provider;
    use std::fs;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn discovers_and_parses_cursor_parent_transcripts() {
        let temp = tempdir().expect("tempdir");
        let root = temp.path().join("projects");
        let session_id = "9f3b844f-072d-4c2a-bdb8-474fe89dbca1";
        let transcript_dir = root
            .join("Users-adamzhao-Desktop-sessiongrep")
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
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].provider, Provider::Cursor);
        assert_eq!(sources[0].path, transcript_path);

        let parsed = adapter.parse(&sources[0]);
        assert_eq!(parsed.session.id, format!("cursor:{session_id}"));
        assert_eq!(parsed.session.provider_session_id, session_id);
        assert_eq!(parsed.session.title.as_deref(), Some("Great, add tests too"));
        assert_eq!(
            parsed.session.summary.as_deref(),
            Some("Make Cursor threads searchable")
        );
        assert_eq!(parsed.session.message_count, Some(3));
        assert!(parsed.transcript_text.contains("Make Cursor threads searchable"));
        assert!(parsed.transcript_text.contains("I will wire a Cursor provider."));
        assert!(!parsed.transcript_text.contains("ReadFile"));
        assert!(!parsed.transcript_text.contains("subagent"));
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
        assert_eq!(cursor_message_text(&value), "Find the billing bug");
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
    fn extracts_tag_body() {
        assert_eq!(
            extract_tag("prefix <user_query>hello</user_query> suffix", "user_query"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn skips_empty_window_workspace() {
        assert!(decode_cursor_project_dir("empty-window").is_none());
    }
}
