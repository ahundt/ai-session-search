//! Compact session inspection built from existing indexed primitives.
//!
//! The goal is not to invent another dashboard. This module answers the first
//! recovery question after a session hit: what was this session about, what
//! evidence should I open next, and which exact commands expand it safely?

use anyhow::Result;
use serde::Serialize;

use crate::db::Db;
use crate::models::{
    FileQuery, MessageFilters, MessageHit, MessageSearchMode, Role, SessionRecord,
};
use crate::refs::{extract_refs_from_text, ref_summary, MessageRef};
use crate::render::Row;
use crate::util::{render_posix_shell_command, truncate_for_display};

/// Internal row budget per evidence slice. Public callers should tune preview size and then
/// follow exact expansion commands rather than balancing several independent limits.
pub const DEFAULT_EVIDENCE_LIMIT: usize = 12;
pub const DEFAULT_PREVIEW_CHARS: usize = 220;

const REF_EVIDENCE_SCAN_LIMIT: usize = DEFAULT_EVIDENCE_LIMIT * 4;
const REF_CANDIDATE_REGEX: &str = r#"https?://|file://|www\.|[[:alnum:].-]+\.[[:alpha:]]{2,}/"#;

#[derive(Debug, Clone, Serialize)]
pub struct SessionInspection {
    pub session: SessionRecord,
    pub user_intent: Vec<MessagePreview>,
    pub tool_activity: Vec<ToolActivity>,
    pub refs: Vec<RefEvidence>,
    pub changed_files: Vec<ChangedFileEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_profile: Option<crate::models::SessionTimeProfile>,
    pub next_commands: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct InspectionOptions {
    pub preview_chars: usize,
    pub include_time_profile: bool,
}

impl Default for InspectionOptions {
    fn default() -> Self {
        Self {
            preview_chars: DEFAULT_PREVIEW_CHARS,
            include_time_profile: false,
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
        .collect::<Result<Vec<_>>>()?;

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
        .collect::<Result<Vec<_>>>()?;

    let refs = db
        .search_messages(
            REF_CANDIDATE_REGEX,
            &MessageFilters {
                session_id: Some(exact.clone()),
                match_mode: MessageSearchMode::Regex,
                limit: REF_EVIDENCE_SCAN_LIMIT,
                ..Default::default()
            },
        )?
        .iter()
        .map(|hit| ref_evidence(hit, options.preview_chars))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .take(DEFAULT_EVIDENCE_LIMIT)
        .collect();

    let changed_files = db
        .file_cross_ref(&FileQuery {
            session_id: Some(exact.clone()),
            limit: DEFAULT_EVIDENCE_LIMIT,
            ..Default::default()
        })?
        .into_iter()
        .map(|row| {
            Ok(ChangedFileEvidence {
                follow_up_command: render_posix_shell_command(&[
                    "aise".to_string(),
                    "files".to_string(),
                    "history".to_string(),
                    row.file_path.clone(),
                    "--session-id".to_string(),
                    exact.clone(),
                    "--format".to_string(),
                    "json".to_string(),
                ])?,
                file_path: row.file_path,
                provider: row.provider.as_str().to_string(),
                edits: row.edits,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let next_commands = vec![
        vec![
            "aise", "messages", "get", &exact, "--type", "user", "--format", "json",
        ],
        vec![
            "aise", "messages", "timeline", &exact, "--refs", "--format", "json",
        ],
        vec!["aise", "show", &exact, "--transcript-lines", "-40"],
        vec![
            "aise",
            "files",
            "cross-ref",
            "--session-id",
            &exact,
            "--format",
            "json",
        ],
    ];
    let next_commands = next_commands
        .into_iter()
        .map(|parts| {
            render_posix_shell_command(&parts.into_iter().map(str::to_string).collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(SessionInspection {
        session,
        user_intent,
        tool_activity,
        refs,
        changed_files,
        time_profile: options
            .include_time_profile
            .then(|| db.session_time_profile(&exact))
            .transpose()?,
        next_commands,
    })
}

pub fn inspection_rows(
    inspection: &SessionInspection,
    options: InspectionOptions,
) -> Vec<InspectionRow> {
    let mut rows = Vec::new();
    let session = &inspection.session;
    if let Some(profile) = &inspection.time_profile {
        push_exact_row(
            rows.as_mut(),
            "time_profile",
            "messages",
            &profile.messages.to_string(),
        );
        push_exact_row(
            rows.as_mut(),
            "time_profile",
            "timestamped_messages",
            &profile.timestamped_messages.to_string(),
        );
        if let Some(span) = profile.observed_span_seconds {
            push_exact_row(
                rows.as_mut(),
                "time_profile",
                "observed_span_seconds",
                &span.to_string(),
            );
        }
        if let Some(gap) = profile.max_message_gap_seconds {
            push_exact_row(
                rows.as_mut(),
                "time_profile",
                "max_message_gap_seconds",
                &gap.to_string(),
            );
        }
    }
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

fn message_preview(hit: &MessageHit, preview_chars: usize) -> Result<MessagePreview> {
    Ok(MessagePreview {
        seq: hit.seq,
        ts: hit.ts.map(|ts| ts.to_rfc3339()),
        chars: hit.content.chars().count(),
        preview: truncate_for_display(&hit.content, preview_chars),
        expand_command: expand_command(hit)?,
    })
}

fn tool_activity(hit: &MessageHit, preview_chars: usize) -> Result<ToolActivity> {
    Ok(ToolActivity {
        seq: hit.seq,
        ts: hit.ts.map(|ts| ts.to_rfc3339()),
        tool_name: hit.tool_name.clone(),
        kind: classify_tool_activity(hit),
        chars: hit.content.chars().count(),
        preview: truncate_for_display(&hit.content, preview_chars),
        expand_command: expand_command(hit)?,
    })
}

fn ref_evidence(hit: &MessageHit, preview_chars: usize) -> Result<Option<RefEvidence>> {
    let refs = extract_refs_from_text(&hit.content, hit.tool_name.as_deref())
        .into_iter()
        .filter(actionable_ref_for_evidence)
        .collect::<Vec<_>>();
    if refs.is_empty() {
        return Ok(None);
    }
    Ok(Some(RefEvidence {
        seq: hit.seq,
        role: hit.role.as_str().to_string(),
        tool_name: hit.tool_name.clone(),
        ref_summary: ref_summary(&refs),
        refs,
        preview: truncate_for_display(&hit.content, preview_chars),
        expand_command: expand_command(hit)?,
    }))
}

fn actionable_ref_for_evidence(item: &MessageRef) -> bool {
    let value = item.value.to_ascii_lowercase();
    value.starts_with("http://")
        || value.starts_with("https://")
        || value.starts_with("file://")
        || value.starts_with("www.")
        || (value.contains('/') && item.host.is_some())
}

fn classify_tool_activity(hit: &MessageHit) -> String {
    match hit.kind {
        crate::models::MessageKind::ToolCall => "call",
        crate::models::MessageKind::ToolResult => "result",
        _ => "other",
    }
    .to_string()
}

fn expand_command(hit: &MessageHit) -> Result<String> {
    render_posix_shell_command(&[
        "aise".to_string(),
        "messages".to_string(),
        "get".to_string(),
        hit.session_id.clone(),
        "--seq".to_string(),
        hit.seq.to_string(),
        "--context".to_string(),
        "3".to_string(),
        "--refs".to_string(),
        "--format".to_string(),
        "json".to_string(),
    ])
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
                message_count: Some(6),
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
                    5,
                    Role::Tool,
                    Some("List"),
                    "local files include app.md settings.json and LINT.IfChange",
                ),
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
        assert!(inspection.tool_activity.len() >= 2);
        assert_eq!(inspection.tool_activity[0].kind, "call");
        assert_eq!(inspection.tool_activity[1].kind, "result");
        let ref_values = inspection
            .refs
            .iter()
            .flat_map(|item| item.refs.iter().map(|item| item.value.as_str()))
            .collect::<Vec<_>>();
        assert!(!ref_values.is_empty());
        assert!(ref_values.contains(&"https://example.com/a"));
        assert!(ref_values.contains(&"docs.rs/linkify"));
        assert!(!ref_values.contains(&"app.md"));
        assert!(!ref_values.contains(&"settings.json"));
        assert_eq!(inspection.changed_files.len(), 1);
        assert!(inspection.next_commands.iter().any(|cmd| {
            cmd == "aise messages timeline claude:test-inspect --refs --format json"
        }));

        let rows = inspection_rows(
            &inspection,
            InspectionOptions {
                preview_chars: 12,
                ..Default::default()
            },
        );
        assert!(rows.iter().any(|row| row.section == "user_intent"));
        assert!(rows
            .iter()
            .any(|row| row.section == "session" && row.key == "source_path"));
        assert!(rows.iter().any(|row| row.section == "next_commands"));
        assert!(rows.iter().any(|row| {
            row.section == "next_commands"
                && row
                    .value
                    .contains("aise messages timeline claude:test-inspect --refs")
        }));
    }

    fn msg(seq: i64, role: Role, tool_name: Option<&str>, content: &str) -> Message {
        let kind = match role {
            Role::Compaction => crate::models::MessageKind::Compaction,
            Role::Tool if content.contains(r#""kind":"tool_call""#) => {
                crate::models::MessageKind::ToolCall
            }
            Role::Tool => crate::models::MessageKind::ToolResult,
            _ => crate::models::MessageKind::Conversation,
        };
        Message {
            seq,
            role,
            ts: None,
            tool_name: tool_name.map(str::to_string),
            kind,
            tool_call_id: None,
            is_compaction: false,
            content: content.to_string(),
        }
    }
}
