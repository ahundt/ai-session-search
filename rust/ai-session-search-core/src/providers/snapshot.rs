use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use chrono::{DateTime, Utc};

use crate::models::{ParsedSession, Provider, SessionRecord, SourceFile};
use crate::util::{format_transcript_line, normalize_path, provider_parse_version, to_messages};

pub(super) type Turn = (String, String, Option<DateTime<Utc>>);

pub(super) struct SnapshotMetadata<'a> {
    pub provider_session_id: &'a str,
    pub cwd: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub discovery_source: &'a str,
}

pub(super) fn source_file(
    provider: Provider,
    path: PathBuf,
    metadata: &fs::Metadata,
) -> SourceFile {
    let mtime_ns = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    SourceFile {
        provider,
        path,
        mtime_ns,
        size_bytes: metadata.len().min(i64::MAX as u64) as i64,
    }
}

pub(super) fn parsed_session(
    provider: Provider,
    path: &Path,
    metadata: SnapshotMetadata<'_>,
    turns: Vec<Turn>,
) -> ParsedSession {
    let messages = to_messages(turns);
    let transcript_text = messages
        .iter()
        .map(|message| format_transcript_line(message.role.as_str(), message.ts, &message.content))
        .collect::<Vec<_>>()
        .join("\n");
    let preview_text = messages
        .first()
        .map(|message| message.content.clone())
        .unwrap_or_default();
    let last_message_at = messages.iter().filter_map(|message| message.ts).max();
    ParsedSession {
        session: SessionRecord {
            id: format!("{}:{}", provider.as_str(), normalize_path(path)),
            provider,
            provider_session_id: metadata.provider_session_id.to_string(),
            title: path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(str::to_string),
            summary: None,
            cwd: metadata.cwd,
            repo_root: None,
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            last_message_at,
            preview_text,
            source_path: normalize_path(path),
            message_count: Some(messages.len() as i64),
            parse_version: provider_parse_version(provider).to_string(),
            raw_metadata_json: None,
            parse_warning: None,
            discovery_source: metadata.discovery_source.to_string(),
            // Snapshot providers (aistudio, gemini-cli) expose one file per session with no
            // spawn marker, so there is no origin to record. See models.rs SessionRecord.
            parent_session_id: None,
            agent_label: None,
        },
        transcript_text,
        messages,
        file_edits: Vec::new(),
    }
}
