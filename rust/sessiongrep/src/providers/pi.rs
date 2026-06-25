use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::{Value, json};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
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
        let raw = fs::read_to_string(path)?;
        let mut provider_session_id = self
            .extract_id(path)
            .unwrap_or_else(|| "unknown".to_string());
        let mut cwd = None;
        let mut created_at: Option<DateTime<Utc>> = None;
        let mut updated_at: Option<DateTime<Utc>> = None;
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
                    if !matches!(role, Some("user") | Some("assistant")) {
                        continue;
                    }
                    let text = extract_text(message);
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    if created_at.is_none() {
                        created_at = timestamp;
                    }
                    updated_at = timestamp.or(updated_at);
                    messages.push((
                        role.unwrap_or("message").to_string(),
                        text.to_string(),
                        timestamp,
                    ));
                    transcript_lines.push(format_transcript_line(
                        role.unwrap_or("message"),
                        timestamp,
                        text,
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
            "line_count": raw.lines().count(),
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
            messages: crate::util::to_messages(messages),
            file_edits: Vec::new(),
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
        assert_eq!(parsed.session.cwd.as_deref(), Some("/Users/nisarg/src/demo"));
        assert_eq!(parsed.session.title.as_deref(), Some("Add pi support to sessiongrep"));
        assert_eq!(parsed.session.message_count, Some(2));
        assert!(parsed.transcript_text.contains("Add pi support to sessiongrep"));
        assert!(parsed.transcript_text.contains("I will wire up a pi adapter."));
        // Thinking and tool payloads stay out of the transcript.
        assert!(!parsed.transcript_text.contains("secret reasoning"));
        assert!(!parsed.transcript_text.contains("toolCall"));
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
}
