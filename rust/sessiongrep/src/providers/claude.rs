use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde_json::{Value, json};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{
    extract_text, find_repo_root, format_transcript_line, minimal_record, normalize_path,
    parse_datetime, preview_from_text, substantive_text, truncate_for_display,
};

pub struct ClaudeAdapter {
    roots: Vec<PathBuf>,
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
                if path.components().any(|component| {
                    let value = component.as_os_str();
                    value == "memory" || value == "subagents"
                }) {
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
                        provider: Provider::Claude,
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
            Err(err) => minimal_record(Provider::Claude, &source.path, err.to_string()),
        }
    }

    fn parse_inner(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)?;
        let mut provider_session_id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
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
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(_) => continue,
            };
            raw_meta.push(value.clone());
            if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
                provider_session_id = session_id.to_string();
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

            let timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);

            let mut role = value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string);
            let mut text = String::new();

            if let Some(message) = value.get("message") {
                role = message
                    .get("role")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or(role);
                text = extract_text(message);
            } else if let Some(message) = value.get("content").and_then(Value::as_str) {
                text = message.to_string();
            }

            if should_skip_message(&value, &text) {
                continue;
            }

            match role.as_deref() {
                Some("user") | Some("assistant") => {
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    if created_at.is_none() {
                        created_at = timestamp;
                    }
                    updated_at = timestamp.or(updated_at);
                    messages.push((role.unwrap_or_default(), text.clone(), timestamp));
                    transcript_lines.push(format_transcript_line(
                        messages
                            .last()
                            .map(|(role, _, _)| role.as_str())
                            .unwrap_or("message"),
                        timestamp,
                        &text,
                    ));
                }
                _ => {}
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
            id: format!("claude:{provider_session_id}"),
            provider: Provider::Claude,
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
            parse_version: "claude-v1".to_string(),
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

fn should_skip_message(value: &Value, text: &str) -> bool {
    let normalized = text.trim();
    let is_meta = value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_local_command_caveat = is_meta
        && (normalized.contains("<local-command-caveat>")
            || normalized.contains(
                "Caveat: The messages below were generated by the user while running local commands.",
            ));
    let is_command_bookkeeping = normalized.contains("<command-name>/exit</command-name>")
        || normalized.contains("<command-name>/resume</command-name>")
        || normalized.contains("<command-name>/clear</command-name>")
        || normalized.contains("<command-name>/compact</command-name>")
        || normalized.eq_ignore_ascii_case("resume cancelled");

    is_local_command_caveat || is_command_bookkeeping
}


#[cfg(test)]
mod tests {
    use super::should_skip_message;
    use serde_json::json;

    #[test]
    fn skips_local_command_caveat_meta_messages() {
        let value = json!({
            "isMeta": true,
            "message": {
                "role": "user",
                "content": "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"
            }
        });
        let text = "<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>";
        assert!(should_skip_message(&value, text));
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
        assert!(!should_skip_message(&value, "real prompt"));
    }

    #[test]
    fn skips_command_bookkeeping_messages() {
        let value = json!({
            "isMeta": false,
            "message": {
                "role": "user",
                "content": "<command-name>/exit</command-name><command-message>exit</command-message>"
            }
        });
        assert!(should_skip_message(
            &value,
            "<command-name>/exit</command-name><command-message>exit</command-message>"
        ));
    }
}
