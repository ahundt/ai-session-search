//! Compact session inspection built from existing indexed primitives.
//!
//! The goal is not to invent another dashboard. This module answers the first
//! recovery question after a session hit: what was this session about, what
//! evidence should I open next, and which exact commands expand it safely?

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;
use crate::models::{FileQuery, MessageFilters, MessageHit, Role, SessionRecord};
use crate::refs::{extract_refs_from_text, ref_summary, MessageRef};
use crate::render::Row;
use crate::util::truncate_for_display;

/// Internal row budget per evidence slice. Public callers should tune preview size and then
/// follow exact expansion commands rather than balancing several independent limits.
pub const DEFAULT_EVIDENCE_LIMIT: usize = 12;
pub const DEFAULT_PREVIEW_CHARS: usize = 220;

const REF_EVIDENCE_SCAN_LIMIT: usize = DEFAULT_EVIDENCE_LIMIT * 4;
const REF_CANDIDATE_REGEX: &str = r#"https?://|file://|www\.|[[:alnum:].-]+\.[[:alpha:]]{2,}"#;

#[derive(Debug, Clone, Serialize)]
pub struct SessionInspection {
    pub session: SessionRecord,
    pub user_intent: Vec<MessagePreview>,
    pub tool_activity: Vec<ToolActivity>,
    pub refs: Vec<RefEvidence>,
    pub changed_files: Vec<ChangedFileEvidence>,
    pub next_commands: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct InspectionOptions {
    pub preview_chars: usize,
}

impl Default for InspectionOptions {
    fn default() -> Self {
        Self {
            preview_chars: DEFAULT_PREVIEW_CHARS,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MessagePreview {
    pub seq: i64,
    pub ts: Option<String>,
    pub chars: usize,
    pub preview: String,
    pub expand_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolActivity {
    pub seq: i64,
    pub ts: Option<String>,
    pub tool_name: Option<String>,
    pub kind: String,
    pub chars: usize,
    pub preview: String,
    pub expand_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RefEvidence {
    pub seq: i64,
    pub role: String,
    pub tool_name: Option<String>,
    pub ref_summary: String,
    pub refs: Vec<MessageRef>,
    pub preview: String,
    pub expand_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChangedFileEvidence {
    pub file_path: String,
    pub provider: String,
    pub edits: i64,
    pub follow_up_command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InspectionRow {
    pub section: String,
    pub key: String,
    pub value: String,
}

impl Row for InspectionRow {
    fn headers() -> &'static [&'static str] {
        &["section", "key", "value"]
    }

    fn cells(&self) -> Vec<String> {
        vec![self.section.clone(), self.key.clone(), self.value.clone()]
    }
}

pub fn inspect_session(
    db: &Db,
    session_id_or_prefix: &str,
    options: InspectionOptions,
) -> Result<SessionInspection> {
    let session = db.resolve_session_record(session_id_or_prefix)?;
    let exact = session.id.clone();

    let user_intent = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::User),
                session_id: Some(exact.clone()),
                limit: DEFAULT_EVIDENCE_LIMIT,
                ..Default::default()
            },
        )?
        .iter()
        .map(|hit| message_preview(hit, options.preview_chars))
        .collect();

    let tool_activity = db
        .search_messages(
            "",
            &MessageFilters {
                role: Some(Role::Tool),
                session_id: Some(exact.clone()),
                limit: DEFAULT_EVIDENCE_LIMIT,
                ..Default::default()
            },
        )?
        .iter()
        .map(|hit| tool_activity(hit, options.preview_chars))
        .collect();

    let refs = db
        .search_messages(
            "",
            &MessageFilters {
                session_id: Some(exact.clone()),
                regex: Some(REF_CANDIDATE_REGEX.to_string()),
                limit: REF_EVIDENCE_SCAN_LIMIT,
                ..Default::default()
            },
        )?
        .iter()
        .filter_map(|hit| ref_evidence(hit, options.preview_chars))
        .take(DEFAULT_EVIDENCE_LIMIT)
        .collect();

    let changed_files = db
        .file_cross_ref(&FileQuery {
            session_id: Some(exact.clone()),
            limit: DEFAULT_EVIDENCE_LIMIT,
            ..Default::default()
        })?
        .into_iter()
        .map(|row| ChangedFileEvidence {
            follow_up_command: format!(
                "sessiongrep files history {} --session-id {} --format json",
                shell_word(&row.file_path),
                shell_word(&exact)
            ),
            file_path: row.file_path,
            provider: row.provider.as_str().to_string(),
            edits: row.edits,
        })
        .collect();

    let next_commands = vec![
        format!(
            "sessiongrep messages get {} --type user --format json",
            shell_word(&exact)
        ),
        format!(
            "sessiongrep messages timeline {} --refs --format json",
            shell_word(&exact)
        ),
        format!("sessiongrep show {} --max-lines -40", shell_word(&exact)),
        format!(
            "sessiongrep files cross-ref --session-id {} --format json",
            shell_word(&exact)
        ),
    ];

    Ok(SessionInspection {
        session,
        user_intent,
        tool_activity,
        refs,
        changed_files,
        next_commands,
    })
}

pub fn inspection_rows(
    inspection: &SessionInspection,
    options: InspectionOptions,
) -> Vec<InspectionRow> {
    let mut rows = Vec::new();
    let session = &inspection.session;
    push_row(&mut rows, "session", "id", &session.id, options);
    push_row(
        &mut rows,
        "session",
        "provider",
        session.provider.as_str(),
        options,
    );
    push_row(
        &mut rows,
        "session",
        "provider_session_id",
        &session.provider_session_id,
        options,
    );
    if let Some(title) = &session.title {
        push_row(&mut rows, "session", "title", title, options);
    }
    if let Some(cwd) = &session.cwd {
        push_row(&mut rows, "session", "cwd", cwd, options);
    }
    if let Some(repo) = &session.repo_root {
        push_row(&mut rows, "session", "repo", repo, options);
    }
    push_exact_row(&mut rows, "session", "source_path", &session.source_path);
    push_row(
        &mut rows,
        "session",
        "discovery_source",
        &session.discovery_source,
        options,
    );
    if let Some(created) = session.created_at {
        push_row(
            &mut rows,
            "session",
            "created_at",
            &created.to_rfc3339(),
            options,
        );
    }
    if let Some(updated) = session.updated_at {
        push_row(
            &mut rows,
            "session",
            "updated_at",
            &updated.to_rfc3339(),
            options,
        );
    }
    if let Some(last_message) = session.last_message_at {
        push_row(
            &mut rows,
            "session",
            "last_message_at",
            &last_message.to_rfc3339(),
            options,
        );
    }
    if let Some(count) = session.message_count {
        push_row(
            &mut rows,
            "session",
            "message_count",
            &count.to_string(),
            options,
        );
    }
    if let Some(warning) = &session.parse_warning {
        push_row(&mut rows, "session", "parse_warning", warning, options);
    }

    for msg in &inspection.user_intent {
        push_row(
            &mut rows,
            "user_intent",
            &format!("seq {}", msg.seq),
            &msg.preview,
            options,
        );
    }
    for tool in &inspection.tool_activity {
        let key = tool
            .tool_name
            .as_deref()
            .map(|name| format!("seq {} {name}", tool.seq))
            .unwrap_or_else(|| format!("seq {}", tool.seq));
        push_row(&mut rows, "tool_activity", &key, &tool.preview, options);
    }
    for item in &inspection.refs {
        push_row(
            &mut rows,
            "refs",
            &format!("seq {} {}", item.seq, item.ref_summary),
            &item.preview,
            options,
        );
    }
    for file in &inspection.changed_files {
        push_row(
            &mut rows,
            "changed_files",
            &format!("{} edits", file.edits),
            &file.file_path,
            options,
        );
    }
    for command in &inspection.next_commands {
        push_exact_row(&mut rows, "next_commands", "expand", command);
    }
    rows
}

fn push_row(
    rows: &mut Vec<InspectionRow>,
    section: &str,
    key: &str,
    value: &str,
    options: InspectionOptions,
) {
    rows.push(InspectionRow {
        section: section.to_string(),
        key: key.to_string(),
        value: truncate_for_display(value, options.preview_chars),
    });
}

fn push_exact_row(rows: &mut Vec<InspectionRow>, section: &str, key: &str, value: &str) {
    rows.push(InspectionRow {
        section: section.to_string(),
        key: key.to_string(),
        value: value.to_string(),
    });
}

fn message_preview(hit: &MessageHit, preview_chars: usize) -> MessagePreview {
    MessagePreview {
        seq: hit.seq,
        ts: hit.ts.map(|ts| ts.to_rfc3339()),
        chars: hit.content.chars().count(),
        preview: truncate_for_display(&hit.content, preview_chars),
        expand_command: expand_command(hit),
    }
}

fn tool_activity(hit: &MessageHit, preview_chars: usize) -> ToolActivity {
    ToolActivity {
        seq: hit.seq,
        ts: hit.ts.map(|ts| ts.to_rfc3339()),
        tool_name: hit.tool_name.clone(),
        kind: classify_tool_activity(hit),
        chars: hit.content.chars().count(),
        preview: truncate_for_display(&hit.content, preview_chars),
        expand_command: expand_command(hit),
    }
}

fn ref_evidence(hit: &MessageHit, preview_chars: usize) -> Option<RefEvidence> {
    let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref());
    if refs.is_empty() {
        return None;
    }
    Some(RefEvidence {
        seq: hit.seq,
        role: hit.role.as_str().to_string(),
        tool_name: hit.tool_name.clone(),
        ref_summary: ref_summary(&refs),
        refs,
        preview: truncate_for_display(&hit.content, preview_chars),
        expand_command: expand_command(hit),
    })
}

fn classify_tool_activity(hit: &MessageHit) -> String {
    if hit.content.contains(r#""kind":"tool_call""#) {
        "call".to_string()
    } else {
        "result".to_string()
    }
}

fn expand_command(hit: &MessageHit) -> String {
    format!(
        "sessiongrep messages get {} --seq {} --context 3 --refs --format json",
        shell_word(&hit.session_id),
        hit.seq
    )
}

fn shell_word(value: &str) -> String {
    shlex::try_quote(value)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| format!("{value:?}"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::models::{FileEdit, Message, ParsedSession, Provider, SessionRecord};

    use super::*;

    #[test]
    fn inspect_session_returns_bounded_evidence_and_followups() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("index.db")).unwrap();
        let parsed = ParsedSession {
            session: SessionRecord {
                id: "claude:test-inspect".to_string(),
                provider: Provider::Claude,
                provider_session_id: "test-inspect".to_string(),
                title: Some("Inspect me".to_string()),
                summary: None,
                cwd: Some("/tmp/project".to_string()),
                repo_root: Some("/tmp/project".to_string()),
                created_at: None,
                updated_at: None,
                last_message_at: None,
                preview_text: "Inspect me".to_string(),
                source_path: Path::new("/tmp/test.jsonl").display().to_string(),
                message_count: Some(5),
                parse_version: "test".to_string(),
                raw_metadata_json: None,
                parse_warning: None,
                discovery_source: "test".to_string(),
            },
            transcript_text: String::new(),
            messages: vec![
                msg(0, Role::User, None, "please inspect https://example.com/a"),
                msg(1, Role::Assistant, None, "ok"),
                msg(4, Role::Assistant, None, "schema docs at docs.rs/linkify"),
                msg(
                    2,
                    Role::Tool,
                    Some("Bash"),
                    r#"{"kind":"tool_call","tool_name":"Bash","args":{"command":"cargo test"}}"#,
                ),
                msg(3, Role::Tool, Some("Bash"), "finished successfully"),
            ],
            file_edits: vec![FileEdit {
                seq: 0,
                ts: None,
                tool: "Write".to_string(),
                file_path: "/tmp/project/src/lib.rs".to_string(),
                file_name: "lib.rs".to_string(),
                new_content: None,
                edits: Vec::new(),
            }],
        };
        db.upsert_session(&parsed, 0, 0).unwrap();

        let inspection =
            inspect_session(&db, "claude:test-inspect", InspectionOptions::default()).unwrap();
        assert_eq!(inspection.session.id, "claude:test-inspect");
        assert_eq!(inspection.user_intent.len(), 1);
        assert!(inspection.user_intent[0].preview.contains("please inspect"));
        assert_eq!(inspection.tool_activity.len(), 2);
        assert_eq!(inspection.tool_activity[0].kind, "call");
        assert_eq!(inspection.tool_activity[1].kind, "result");
        assert_eq!(inspection.refs.len(), 2);
        let ref_values = inspection
            .refs
            .iter()
            .flat_map(|item| item.refs.iter().map(|item| item.value.as_str()))
            .collect::<Vec<_>>();
        assert!(ref_values.contains(&"https://example.com/a"));
        assert!(ref_values.contains(&"docs.rs/linkify"));
        assert_eq!(inspection.changed_files.len(), 1);
        assert!(inspection.next_commands.iter().any(|cmd| {
            cmd == "sessiongrep messages timeline claude:test-inspect --refs --format json"
        }));

        let rows = inspection_rows(&inspection, InspectionOptions { preview_chars: 12 });
        assert!(rows.iter().any(|row| row.section == "user_intent"));
        assert!(rows
            .iter()
            .any(|row| row.section == "session" && row.key == "source_path"));
        assert!(rows.iter().any(|row| row.section == "next_commands"));
        assert!(rows.iter().any(|row| {
            row.section == "next_commands"
                && row
                    .value
                    .contains("sessiongrep messages timeline claude:test-inspect --refs")
        }));
    }

    fn msg(seq: i64, role: Role, tool_name: Option<&str>, content: &str) -> Message {
        Message {
            seq,
            role,
            ts: None,
            tool_name: tool_name.map(str::to_string),
            is_compaction: false,
            content: content.to_string(),
        }
    }
}
