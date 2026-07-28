use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::models::{MessageCorrelationAuthority, ParsedSession, Provider, SourceFile};
use crate::providers::snapshot::{parsed_session_from_raw, source_file, SnapshotMetadata};
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
        let mut sources = Vec::new();
        for root in &self.roots {
            let Ok(hash_dirs) = fs::read_dir(root) else {
                continue;
            };
            for hash_dir in hash_dirs.flatten().filter(|entry| entry.path().is_dir()) {
                let Ok(entries) = fs::read_dir(hash_dir.path().join("chats")) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let supported = path
                        .file_name()
                        .and_then(|value| value.to_str())
                        .is_some_and(|name| {
                            name.starts_with("session-") && name.ends_with(".json")
                        });
                    if supported && path.is_file() {
                        if let Ok(metadata) = entry.metadata() {
                            sources.push(source_file(Provider::GeminiCli, path, &metadata));
                        }
                    }
                }
            }
        }
        sources.sort_by(|left, right| left.path.cmp(&right.path));
        sources
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        self.parse_path(&source.path).unwrap_or_else(|error| {
            minimal_record(Provider::GeminiCli, &source.path, format!("{error:#}"))
        })
    }

    fn parse_path(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read Gemini CLI session {}", path.display()))?;
        let data: Value = serde_json::from_str(&raw).context("invalid Gemini CLI JSON")?;
        let strip_references = referenced_file_block_regex()?;
        let mut messages = Vec::new();
        for message in data
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let role = match message.get("type").and_then(Value::as_str) {
                Some("user") => "user",
                Some("gemini") => "assistant",
                _ => continue,
            };
            let content = content_text(message.get("content"));
            let content = strip_references
                .replace_all(&content, "")
                .trim()
                .to_string();
            if content.is_empty() {
                continue;
            }
            let timestamp = message
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_datetime);
            let mut raw_message = RawMessage::message(role, content, timestamp, None);
            if let Some(event_id) = message
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.trim().is_empty())
            {
                raw_message = raw_message
                    .with_native_event_identity(MessageCorrelationAuthority::Google, event_id);
            }
            messages.push(raw_message);
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
        .map(|path| (format!("{:x}", Sha256::digest(path.as_bytes())), path))
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
        let hash = format!("{:x}", Sha256::digest(project.as_bytes()));
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
            crate::models::MessageAuthorship::Unknown,
            "Gemini CLI user-role rows can contain injected context and lack origin evidence"
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
