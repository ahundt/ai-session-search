// SPDX-FileCopyrightText: 2026 Andrew Hundt
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::Value;

use crate::models::{ParsedSession, Provider, SourceFile};
use crate::providers::snapshot::{parsed_session, source_file, SnapshotMetadata, Turn};
use crate::providers::{walk_roots, ProviderDiscovery};
use crate::util::minimal_record;

pub struct AiStudioAdapter {
    roots: Vec<PathBuf>,
}

impl AiStudioAdapter {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    pub fn discover(&self) -> Vec<SourceFile> {
        self.discover_with_warnings().sources
    }

    pub(crate) fn discover_with_warnings(&self) -> ProviderDiscovery {
        let mut sources = Vec::new();
        let walked = walk_roots(&self.roots, Some(1));
        for entry in walked.entries {
            let path = &entry.path;
            let supported = path
                .extension()
                .and_then(|value| value.to_str())
                .is_some_and(|extension| {
                    extension.eq_ignore_ascii_case("json") || extension.eq_ignore_ascii_case("md")
                });
            if !supported || !entry.metadata.is_file() {
                continue;
            }
            sources.push(source_file(Provider::AiStudio, entry.path, &entry.metadata));
        }
        ProviderDiscovery {
            sources,
            warnings: walked.warnings,
        }
    }

    pub fn parse(&self, source: &SourceFile) -> ParsedSession {
        self.parse_path(&source.path).unwrap_or_else(|error| {
            minimal_record(Provider::AiStudio, &source.path, format!("{error:#}"))
        })
    }

    fn parse_path(&self, path: &Path) -> Result<ParsedSession> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read AI Studio session {}", path.display()))?;
        self.parse_raw(path, raw)
    }

    pub(crate) fn parse_raw(&self, path: &Path, raw: String) -> Result<ParsedSession> {
        let mut turns: Vec<Turn> = Vec::new();
        let discovery_source = if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            if !raw.trim().is_empty() {
                turns.push(("user".to_string(), raw.clone(), None));
            }
            "markdown"
        } else {
            let data: Value = serde_json::from_str(&raw).context("invalid AI Studio JSON")?;
            for chunk in data
                .pointer("/chunkedPrompt/chunks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let role = match chunk.get("role").and_then(Value::as_str) {
                    Some("user") => "user",
                    Some("model" | "assistant") => "assistant",
                    _ => continue,
                };
                let Some(text) = chunk
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                else {
                    continue;
                };
                turns.push((role.to_string(), text.to_string(), None));
            }
            "json"
        };
        let provider_id = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("session");
        Ok(parsed_session(
            Provider::AiStudio,
            path,
            SnapshotMetadata {
                provider_session_id: provider_id,
                cwd: None,
                created_at: None,
                updated_at: None,
                discovery_source,
            },
            turns,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_json_and_markdown_and_normalizes_roles() {
        let dir = tempfile::tempdir().unwrap();
        let json = dir.path().join("chat.json");
        fs::write(
            &json,
            r#"{"chunkedPrompt":{"chunks":[{"role":"user","text":"hello"},{"role":"model","text":"hi"},{"role":"other","text":"skip"}]}}"#,
        )
        .unwrap();
        fs::write(dir.path().join("legacy.md"), "legacy prompt").unwrap();
        fs::write(dir.path().join("ignored.gif"), "not an image").unwrap();
        let adapter = AiStudioAdapter::new(vec![dir.path().to_path_buf()]);

        let sources = adapter.discover();
        let parsed = adapter.parse(sources.iter().find(|source| source.path == json).unwrap());

        assert_eq!(sources.len(), 2);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].content, "hello");
        assert_eq!(parsed.messages[1].content, "hi");
        assert_eq!(parsed.session.provider, Provider::AiStudio);
        assert_eq!(
            parsed.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Human,
            "an AI Studio export is one person-started chat per file; its user turns are the \
             person's prompts"
        );
        assert_eq!(
            parsed.messages[1].provenance.authorship,
            crate::models::MessageAuthorship::Agent
        );
        assert!(parsed
            .messages
            .iter()
            .all(|message| message.provenance.correlation_identity.is_none()));

        let markdown_source = sources
            .iter()
            .find(|source| source.path.ends_with("legacy.md"))
            .expect("markdown source");
        let markdown = adapter.parse(markdown_source);
        assert_eq!(
            markdown.messages[0].provenance.authorship,
            crate::models::MessageAuthorship::Human
        );
    }

    #[test]
    fn malformed_json_returns_minimal_warning_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        fs::write(&path, "{").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let source = source_file(Provider::AiStudio, path, &metadata);

        let parsed = AiStudioAdapter::new(Vec::new()).parse(&source);

        let warning = parsed.session.parse_warning.as_deref().unwrap();
        assert!(warning.contains("invalid AI Studio JSON"), "{warning}");
        assert!(warning.contains("line 1 column 1"), "{warning}");
        assert!(parsed.messages.is_empty());
    }
}
